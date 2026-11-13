//! PDQ perceptual image hashing.
//!
//! A port of Meta's open PDQ algorithm as published in the ThreatExchange
//! repository (`pdq/cpp`). The picture is reduced to luminance, blurred with a
//! Jarosz tent filter and decimated to 64x64, transformed with a 16x16 slice
//! of the DCT, and each of the 256 coefficients is compared with their median
//! to produce one bit. Pictures that look alike produce hashes a short Hamming
//! distance apart; Meta suggests treating 31 or fewer differing bits as a
//! match and discarding hashes whose quality is 49 or lower.
//!
//! Bit order and the hex form follow the reference: bit `k` is coefficient
//! `k` in row-major order, and the 64 hex digits read as one big-endian
//! 256-bit number in which bit `k` has weight `2^k`.

use std::{f64::consts::PI, sync::LazyLock};

use image::DynamicImage;

/// Side of the blurred and decimated luminance image the DCT reads.
const BLOCK: usize = 64;

/// Side of the slice of DCT coefficients the hash is taken from.
const COEFFS: usize = 16;

/// Zero-based rank of the median among the 256 coefficients: the 128th
/// smallest, which is what the reference's Torben selection returns.
const MEDIAN_INDEX: usize = 127;

/// Pictures smaller than this along either side hash to all zeroes.
const MIN_HASHABLE_DIM: usize = 5;

/// Number of horizontal and vertical box passes making up the tent filter.
const JAROSZ_PASSES: usize = 2;

/// Pictures larger than this along either side are scaled down first, which
/// keeps hashing cheap without moving the result meaningfully.
const MAX_SIDE: u32 = 512;

/// Standard RGB to luminance weights, as the reference uses them.
const LUMA_R: f32 = 0.299;
const LUMA_G: f32 = 0.587;
const LUMA_B: f32 = 0.114;

/// The first 16 rows (frequencies 1 to 16) of the 64-point DCT-II matrix,
/// computed in double precision and stored as single precision exactly as
/// the reference does.
static DCT_MATRIX: LazyLock<[[f32; BLOCK]; COEFFS]> = LazyLock::new(dct_matrix);

/// A picture's PDQ hash and how much detail it was taken from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Pdq {
	/// The 256-bit hash, most significant byte first, as its hex form reads.
	pub hash: [u8; 32],

	/// How much gradient the picture has, from 0 (flat) to 100.
	pub quality: u8,
}

/// Hashes a decoded picture.
///
/// Pictures larger than 512 pixels along a side are first scaled to fit, and
/// pictures smaller than 5 pixels along a side yield an all-zero hash with no
/// quality.
pub(crate) fn pdq(image: &DynamicImage) -> Pdq {
	let scaled;
	let image = if image.width() > MAX_SIDE || image.height() > MAX_SIDE {
		scaled = image.thumbnail(MAX_SIDE, MAX_SIDE);
		&scaled
	} else {
		image
	};

	let (luma, rows, cols) = luminance(image);
	hash_luma(luma, rows, cols)
}

/// Formats a hash as the 64 lowercase hex digits other PDQ tools exchange.
#[cfg_attr(not(test), expect(dead_code, reason = "the known-bad list keeps its own hex; these pin the format in tests"))]
pub(crate) fn to_hex(hash: &[u8; 32]) -> String {
	const DIGITS: &[u8; 16] = b"0123456789abcdef";

	hash.iter()
		.flat_map(|&byte| [byte >> 4, byte & 0x0F])
		.filter_map(|nibble| DIGITS.get(usize::from(nibble)))
		.map(|&digit| char::from(digit))
		.collect()
}

/// Parses the 64 hex digits of a hash, in either case.
///
/// Anything else, including surrounding whitespace, is refused.
#[cfg_attr(not(test), expect(dead_code, reason = "the known-bad list keeps its own hex; these pin the format in tests"))]
pub(crate) fn from_hex(s: &str) -> Option<[u8; 32]> {
	let bytes = s.as_bytes();
	if bytes.len() != 64 {
		return None;
	}

	let mut hash = [0_u8; 32];
	for (out, pair) in hash.iter_mut().zip(bytes.chunks_exact(2)) {
		let (&high, &low) = (pair.first()?, pair.get(1)?);
		let high = char::from(high).to_digit(16)?;
		let low = char::from(low).to_digit(16)?;
		*out = u8::try_from((high << 4) | low).ok()?;
	}

	Some(hash)
}

/// Counts the bits two hashes differ in.
pub(crate) fn distance(a: &[u8; 32], b: &[u8; 32]) -> u32 {
	a.iter()
		.zip(b)
		.map(|(a, b)| (a ^ b).count_ones())
		.sum()
}

/// Reduces a picture to row-major luminance, with its row and column counts.
///
/// Grey pictures are read as they are; everything else is weighted from RGB,
/// with any alpha ignored. The weighting is not fused into multiply-adds,
/// which would round differently from the reference.
#[expect(clippy::suboptimal_flops)]
fn luminance(image: &DynamicImage) -> (Vec<f32>, usize, usize) {
	let rows = usize::try_from(image.height()).unwrap_or(0);
	let cols = usize::try_from(image.width()).unwrap_or(0);
	let luma = match image {
		| DynamicImage::ImageLuma8(grey) => grey
			.as_raw()
			.iter()
			.copied()
			.map(f32::from)
			.collect(),
		| DynamicImage::ImageLumaA8(grey) => grey
			.pixels()
			.map(|pixel| f32::from(pixel.0[0]))
			.collect(),
		| _ => image
			.to_rgb8()
			.pixels()
			.map(|pixel| {
				let [r, g, b] = pixel.0;
				LUMA_R * f32::from(r) + LUMA_G * f32::from(g) + LUMA_B * f32::from(b)
			})
			.collect(),
	};

	(luma, rows, cols)
}

/// Hashes a row-major luminance buffer of the given shape.
///
/// The 16 KiB 64x64 working image stays on the stack.
#[expect(clippy::large_stack_arrays)]
fn hash_luma(mut luma: Vec<f32>, rows: usize, cols: usize) -> Pdq {
	if rows < MIN_HASHABLE_DIM
		|| cols < MIN_HASHABLE_DIM
		|| luma.len() != rows.saturating_mul(cols)
	{
		return Pdq { hash: [0; 32], quality: 0 };
	}

	let mut block = [[0.0_f32; BLOCK]; BLOCK];
	if rows == BLOCK && cols == BLOCK {
		for (row, chunk) in block.iter_mut().zip(luma.chunks_exact(BLOCK)) {
			row.copy_from_slice(chunk);
		}
	} else {
		let mut scratch = vec![0.0_f32; luma.len()];
		let window_along_rows = jarosz_window(cols);
		let window_along_cols = jarosz_window(rows);
		for _ in 0..JAROSZ_PASSES {
			box_along_rows(&luma, &mut scratch, cols, window_along_rows);
			box_along_cols(&scratch, &mut luma, cols, window_along_cols);
		}

		decimate(&luma, rows, cols, &mut block);
	}

	let quality = quality(&block);
	let coeffs = dct_64_to_16(&block);

	Pdq { hash: bits(&coeffs), quality }
}

/// The box window for one pass along a side, half of what one cell of the
/// 64x64 decimation covers, rounded up.
fn jarosz_window(dimension: usize) -> usize { dimension.div_ceil(BLOCK.saturating_mul(2)) }

/// One box-filter pass along every row.
fn box_along_rows(input: &[f32], output: &mut [f32], cols: usize, window: usize) {
	for (input, output) in input
		.chunks_exact(cols)
		.zip(output.chunks_exact_mut(cols))
	{
		box_1d(input, output, window);
	}
}

/// One box-filter pass along every column.
fn box_along_cols(input: &[f32], output: &mut [f32], cols: usize, window: usize) {
	let rows = input.len().checked_div(cols).unwrap_or(0);
	let mut column = Vec::with_capacity(rows);
	let mut filtered = vec![0.0_f32; rows];
	for col in 0..cols {
		column.clear();
		column.extend(input.iter().skip(col).step_by(cols));
		box_1d(&column, &mut filtered, window);
		for (out, &value) in output
			.iter_mut()
			.skip(col)
			.step_by(cols)
			.zip(&filtered)
		{
			*out = value;
		}
	}
}

/// A centred moving average, mirroring the reference's four phases so the
/// same floating-point operations happen in the same order.
///
/// The window shrinks at both ends rather than padding, so the edges are
/// averaged over the samples that exist.
#[expect(clippy::arithmetic_side_effects)]
fn box_1d(input: &[f32], output: &mut [f32], window: usize) {
	let len = input.len().min(output.len());
	let window = window.clamp(1, len.max(1));
	if len == 0 {
		return;
	}

	// Window sizes are at least 1 and at most len, so none of these wrap and
	// the read and write cursors stay within the vector.
	let half = window / 2 + 1;
	let (mut sum, mut count) = (0.0_f32, 0.0_f32);
	let (mut left, mut right, mut out) = (0_usize, 0_usize, 0_usize);

	// Accumulate the leading half window without writing.
	for _ in 0..half - 1 {
		sum += input[right];
		count += 1.0;
		right += 1;
	}

	// Write while the window grows to full size.
	for _ in 0..=window - half {
		sum += input[right];
		count += 1.0;
		output[out] = sum / count;
		right += 1;
		out += 1;
	}

	// Slide the full window.
	for _ in 0..len - window {
		sum += input[right];
		sum -= input[left];
		output[out] = sum / count;
		left += 1;
		right += 1;
		out += 1;
	}

	// Write while the window shrinks off the far end.
	for _ in 0..half - 1 {
		sum -= input[left];
		count -= 1.0;
		output[out] = sum / count;
		left += 1;
		out += 1;
	}
}

/// Samples the blurred buffer at the centre of each of the 64x64 cells.
///
/// The reference computes `floor((i + 0.5) * n / 64)` in double precision,
/// which this integer form reproduces exactly.
fn decimate(input: &[f32], rows: usize, cols: usize, block: &mut [[f32; BLOCK]; BLOCK]) {
	let centre = |index: usize, extent: usize| {
		index
			.saturating_mul(2)
			.saturating_add(1)
			.saturating_mul(extent)
			/ (BLOCK * 2)
	};

	for (i, row) in block.iter_mut().enumerate() {
		let base = centre(i, rows).saturating_mul(cols);
		for (j, cell) in row.iter_mut().enumerate() {
			let index = base.saturating_add(centre(j, cols));
			*cell = input.get(index).copied().unwrap_or(0.0);
		}
	}
}

/// The reference's heuristic quality score: the sum of quantised vertical
/// and horizontal gradients of the 64x64 image, scaled and capped at 100.
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn quality(block: &[[f32; BLOCK]; BLOCK]) -> u8 {
	// Truncates toward zero as the reference's float to int conversion does.
	let step = |u: f32, v: f32| (((u - v) * 100.0) / 255.0) as i32;

	let vertical = block
		.iter()
		.zip(block.iter().skip(1))
		.flat_map(|(upper, lower)| upper.iter().zip(lower))
		.map(|(&u, &v)| step(u, v).unsigned_abs());

	let horizontal = block
		.iter()
		.flat_map(|row| row.iter().zip(row.iter().skip(1)))
		.map(|(&u, &v)| step(u, v).unsigned_abs());

	let sum = vertical
		.chain(horizontal)
		.fold(0_u32, u32::saturating_add);

	u8::try_from((sum / 90).min(100)).unwrap_or(100)
}

/// Builds the 16x64 slice of the DCT-II matrix the hash uses.
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn dct_matrix() -> [[f32; BLOCK]; COEFFS] {
	let scale = (2.0_f64 / 64.0).sqrt() as f32;
	let mut matrix = [[0.0_f32; BLOCK]; COEFFS];
	for (frequency, row) in (1_u32..).zip(matrix.iter_mut()) {
		for (sample, cell) in (0_u32..).zip(row.iter_mut()) {
			let odd = f64::from(sample.saturating_mul(2).saturating_add(1));
			let angle = (PI / 2.0 / 64.0) * f64::from(frequency) * odd;
			*cell = (f64::from(scale) * angle.cos()) as f32;
		}
	}

	matrix
}

/// Computes `D * A * Dt`: the low 16x16 frequencies of the 64x64 image,
/// summing in the same order as the reference and without fused
/// multiply-adds, so the rounding matches it too.
#[expect(clippy::suboptimal_flops)]
fn dct_64_to_16(block: &[[f32; BLOCK]; BLOCK]) -> [[f32; COEFFS]; COEFFS] {
	let matrix = &*DCT_MATRIX;

	let mut partial = [[0.0_f32; BLOCK]; COEFFS];
	for (basis, out_row) in matrix.iter().zip(partial.iter_mut()) {
		for (j, out) in out_row.iter_mut().enumerate() {
			*out = basis
				.iter()
				.zip(block)
				.fold(0.0_f32, |sum, (&d, row)| sum + d * row[j]);
		}
	}

	let mut coeffs = [[0.0_f32; COEFFS]; COEFFS];
	for (partial_row, out_row) in partial.iter().zip(coeffs.iter_mut()) {
		for (basis, out) in matrix.iter().zip(out_row.iter_mut()) {
			*out = partial_row
				.iter()
				.zip(basis)
				.fold(0.0_f32, |sum, (&t, &d)| sum + t * d);
		}
	}

	coeffs
}

/// Sets bit `i * 16 + j` wherever coefficient `(i, j)` exceeds the median.
///
/// The median is taken at [`MEDIAN_INDEX`].
fn bits(coeffs: &[[f32; COEFFS]; COEFFS]) -> [u8; 32] {
	let flat: Vec<f32> = coeffs.iter().flatten().copied().collect();

	let mut sorted = flat.clone();
	let (_, &mut median, _) = sorted.select_nth_unstable_by(MEDIAN_INDEX, f32::total_cmp);

	// Bit k lives in byte 31 - k / 8, at weight 2^(k % 8), so the bytes read
	// most significant first like the reference's hex form.
	let mut hash = [0_u8; 32];
	for (byte, chunk) in hash.iter_mut().rev().zip(flat.chunks_exact(8)) {
		*byte = chunk
			.iter()
			.rev()
			.fold(0_u8, |acc, &coeff| (acc << 1) | u8::from(coeff > median));
	}

	hash
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use image::{DynamicImage, GrayImage, ImageFormat, Rgb, RgbImage, imageops::FilterType};

	use super::{distance, from_hex, pdq, to_hex};

	/// A picture with smooth shading and a few hard-edged shapes, standing in
	/// for a photograph.
	#[expect(
		clippy::as_conversions,
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss,
		clippy::suboptimal_flops
	)]
	fn scene(width: u32, height: u32) -> DynamicImage {
		DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
			let (fx, fy) = (f64::from(x) / f64::from(width), f64::from(y) / f64::from(height));
			let in_disc = (fx - 0.3).powi(2) + (fy - 0.4).powi(2) < 0.04;
			let in_bar = fx > 0.6 && fx < 0.85 && fy > 0.2 && fy < 0.9;
			let shade = |v: f64| u8::try_from((v * 255.0).clamp(0.0, 255.0) as u32).unwrap();
			if in_disc {
				Rgb([shade(0.9), shade(0.2 + fy * 0.3), shade(0.1)])
			} else if in_bar {
				Rgb([shade(0.1), shade(0.3), shade(0.8 - fx * 0.4)])
			} else {
				Rgb([shade(fx * 0.6), shade(0.4 + fy * 0.4), shade(1.0 - fx * 0.5)])
			}
		}))
	}

	#[test]
	fn hex_round_trip() {
		let hash: [u8; 32] = std::array::from_fn(|i| u8::try_from(i * 37 % 256).unwrap());
		let hex = to_hex(&hash);
		assert_eq!(hex.len(), 64, "hex is 64 digits");
		assert_eq!(hex, hex.to_lowercase(), "hex is lowercase");
		assert!(hex.starts_with("0025"), "first byte comes first: {hex}");
		assert_eq!(from_hex(&hex), Some(hash), "round trip");
		assert_eq!(from_hex(&hex.to_uppercase()), Some(hash), "uppercase parses");
		let short = hex.get(..63).unwrap();
		assert_eq!(from_hex(short), None, "short refused");
		assert_eq!(from_hex(&format!("{short}g")), None, "non-hex refused");
		assert_eq!(from_hex(&format!(" {short}")), None, "space refused");
	}

	#[test]
	fn distance_counts_bits() {
		let zero = [0_u8; 32];
		let mut other = zero;
		other[0] = 0b1010_0000;
		other[31] = 0xFF;
		assert_eq!(distance(&zero, &zero), 0, "identical");
		assert_eq!(distance(&zero, &other), 10, "ten bits differ");
		assert_eq!(distance(&zero, &[0xFF; 32]), 256, "all bits differ");
	}

	#[test]
	fn identical_images_match_exactly() {
		let image = scene(300, 200);
		let first = pdq(&image);
		let second = pdq(&scene(300, 200));
		assert_eq!(distance(&first.hash, &second.hash), 0, "same picture, same hash");
		assert_eq!(first, second, "same quality too");
		assert!(first.quality > 0, "a scene has gradients: {}", first.quality);
		let ones: u32 = first.hash.iter().map(|b| b.count_ones()).sum();
		assert!((120..=136).contains(&ones), "about half the bits are set: {ones}");
	}

	#[test]
	fn resized_reencoded_copy_matches() {
		let image = scene(640, 480);
		let original = pdq(&image);

		let resized = image.resize_exact(451, 338, FilterType::Lanczos3);
		let mut jpeg = Vec::new();
		resized
			.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
			.unwrap();
		let decoded = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).unwrap();
		let copy = pdq(&decoded);

		let apart = distance(&original.hash, &copy.hash);
		assert!(apart <= 31, "resized JPEG copy within match distance: {apart}");
	}

	#[test]
	fn different_images_are_far_apart() {
		let gradient = DynamicImage::ImageLuma8(GrayImage::from_fn(256, 256, |x, y| {
			image::Luma([u8::try_from(x.midpoint(y)).unwrap()])
		}));
		let checkerboard = DynamicImage::ImageLuma8(GrayImage::from_fn(256, 256, |x, y| {
			image::Luma([if (x / 32 + y / 32) % 2 == 0 { 0 } else { 255 }])
		}));

		let apart = distance(&pdq(&gradient).hash, &pdq(&checkerboard).hash);
		assert!(apart > 64, "unrelated pictures far apart: {apart}");

		let apart = distance(&pdq(&scene(300, 200)).hash, &pdq(&checkerboard).hash);
		assert!(apart > 64, "scene and checkerboard far apart: {apart}");
	}

	#[test]
	fn tiny_and_flat_images() {
		let tiny = DynamicImage::ImageRgb8(RgbImage::new(4, 100));
		let hashed = pdq(&tiny);
		assert_eq!(hashed.hash, [0; 32], "too small to hash");
		assert_eq!(hashed.quality, 0, "too small to have quality");

		let flat = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb([90, 90, 90])));
		assert_eq!(pdq(&flat).quality, 0, "a flat picture has no quality");
	}

	#[test]
	fn large_images_are_scaled_first() {
		let large = scene(2000, 1500);
		let small = scene(400, 300);
		let apart = distance(&pdq(&large).hash, &pdq(&small).hash);
		assert!(apart <= 31, "same scene at different sizes matches: {apart}");
	}
}
