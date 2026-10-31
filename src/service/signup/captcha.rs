//! The sign-up page's picture captcha, drawn here so nothing is loaded from
//! anyone else. Each letter is a set of strokes, bent, turned and wobbled on
//! its own, under a few lines and specks that cross them.

use std::f32::consts::TAU;

use rand::{RngExt, rng};

/// Letters that can't be mistaken for each other (no O/0, I/1, S/5, Z/2, B/8,
/// G/6 or Q).
pub(super) const ALPHABET: &[u8] = b"ACDEFHJKLMNPRTUVWXY34679";
pub(super) const LENGTH: usize = 6;

const WIDTH: usize = 260;
const HEIGHT: usize = 90;

type Point = (f32, f32);

/// Draws `answer` as a 4-bit grayscale PNG.
pub(super) fn draw(answer: &str) -> Vec<u8> {
	let mut rng = rng();
	let mut ink = vec![0.0_f32; WIDTH * HEIGHT];

	let wave = Wave {
		amp_y: rng.random_range(3.0..6.0),
		freq_y: rng.random_range(0.025..0.045),
		phase_y: rng.random_range(0.0..TAU),
		amp_x: rng.random_range(1.5..3.5),
		freq_x: rng.random_range(0.05..0.09),
		phase_x: rng.random_range(0.0..TAU),
	};

	let count = answer.len().max(1);
	let step = (WIDTH as f32 - 40.0) / count as f32;
	for (i, c) in answer.bytes().enumerate() {
		let height = rng.random_range(38.0..48.0);
		let width = height * rng.random_range(0.55..0.72);
		let centre = (
			20.0 + step * (i as f32 + 0.5) + rng.random_range(-4.0..4.0),
			HEIGHT as f32 / 2.0 + rng.random_range(-8.0..8.0),
		);
		let turn = rng.random_range(-0.35_f32..0.35);
		let shear = rng.random_range(-0.3..0.3);
		let thick = rng.random_range(2.4..3.4);
		let place = |(x, y): Point| {
			let (x, y) = ((x - 0.5) * width, (y - 0.5) * height);
			let x = x + shear * y;
			let (sin, cos) = turn.sin_cos();
			(centre.0 + x * cos - y * sin, centre.1 + x * sin + y * cos)
		};

		for stroke in glyph(c) {
			let points: Vec<Point> = stroke.iter().copied().map(place).collect();
			polyline(&mut ink, &points, thick, &wave);
		}
	}

	// Lines across the letters, about as thick as them, so they can't simply
	// be filtered out by width.
	for _ in 0..3 {
		let y0 = rng.random_range(15.0..HEIGHT as f32 - 15.0);
		let y1 = rng.random_range(15.0..HEIGHT as f32 - 15.0);
		let bend = rng.random_range(-14.0..14.0);
		let points: Vec<Point> = (0..=26)
			.map(|i| {
				let t = i as f32 / 26.0;
				let x = t * WIDTH as f32;
				let y = y0 + (y1 - y0) * t + bend * (t * std::f32::consts::PI).sin();
				(x, y)
			})
			.collect();

		polyline(&mut ink, &points, rng.random_range(1.4..2.4), &wave);
	}

	for _ in 0..220 {
		let x = rng.random_range(0..WIDTH);
		let y = rng.random_range(0..HEIGHT);
		let px = &mut ink[y * WIDTH + x];
		*px = px.max(rng.random_range(0.3..0.9));
	}

	let pixels: Vec<u8> = ink
		.iter()
		.map(|&v| {
			let speck = rng.random_range(0.0..0.12);
			let level = (1.0 - v.min(1.0) * 0.85 - speck).clamp(0.0, 1.0);
			(level * 15.0).round() as u8
		})
		.collect();

	png_gray4(WIDTH, HEIGHT, &pixels)
}

struct Wave {
	amp_y: f32,
	freq_y: f32,
	phase_y: f32,
	amp_x: f32,
	freq_x: f32,
	phase_x: f32,
}

impl Wave {
	fn bend(&self, (x, y): Point) -> Point {
		(
			x + self.amp_x * (y * self.freq_x + self.phase_x).sin(),
			y + self.amp_y * (x * self.freq_y + self.phase_y).sin(),
		)
	}
}

/// Strokes the line through `points`, cut into short pieces so the wave bends
/// it rather than only moving its ends.
fn polyline(ink: &mut [f32], points: &[Point], thick: f32, wave: &Wave) {
	for pair in points.windows(2) {
		let (a, b) = (pair[0], pair[1]);
		let length = (b.0 - a.0).hypot(b.1 - a.1);
		let pieces = (length / 3.0).ceil().max(1.0) as usize;
		let at = |i: usize| {
			let t = i as f32 / pieces as f32;
			wave.bend((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t))
		};

		for i in 0..pieces {
			segment(ink, at(i), at(i + 1), thick);
		}
	}
}

/// Anti-aliased line of width `thick`, merged into `ink` by taking the darker.
fn segment(ink: &mut [f32], a: Point, b: Point, thick: f32) {
	let reach = thick / 2.0 + 1.0;
	let x0 = (a.0.min(b.0) - reach).floor().max(0.0) as usize;
	let y0 = (a.1.min(b.1) - reach).floor().max(0.0) as usize;
	let x1 = ((a.0.max(b.0) + reach).ceil() as usize).min(WIDTH);
	let y1 = ((a.1.max(b.1) + reach).ceil() as usize).min(HEIGHT);
	let (dx, dy) = (b.0 - a.0, b.1 - a.1);
	let len2 = dx * dx + dy * dy;

	for y in y0..y1 {
		for x in x0..x1 {
			let p = (x as f32 + 0.5, y as f32 + 0.5);
			let t = if len2 > 0.0 {
				(((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
			} else {
				0.0
			};
			let d = (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy);
			let v = (thick / 2.0 + 0.5 - d).clamp(0.0, 1.0);
			let px = &mut ink[y * WIDTH + x];
			*px = px.max(v);
		}
	}
}

/// Each letter as strokes in a unit box, y down.
fn glyph(c: u8) -> Vec<Vec<Point>> {
	let p = |pts: &[Point]| pts.to_vec();
	match c {
		| b'A' => vec![p(&[(0.0, 1.0), (0.5, 0.0), (1.0, 1.0)]), p(&[(0.2, 0.62), (0.8, 0.62)])],
		| b'C' => vec![p(&[
			(1.0, 0.15),
			(0.7, 0.0),
			(0.3, 0.0),
			(0.0, 0.3),
			(0.0, 0.7),
			(0.3, 1.0),
			(0.7, 1.0),
			(1.0, 0.85),
		])],
		| b'D' => vec![p(&[
			(0.0, 0.0),
			(0.0, 1.0),
			(0.6, 1.0),
			(1.0, 0.7),
			(1.0, 0.3),
			(0.6, 0.0),
			(0.0, 0.0),
		])],
		| b'E' => vec![
			p(&[(1.0, 0.0), (0.0, 0.0), (0.0, 1.0), (1.0, 1.0)]),
			p(&[(0.0, 0.5), (0.7, 0.5)]),
		],
		| b'F' => vec![p(&[(1.0, 0.0), (0.0, 0.0), (0.0, 1.0)]), p(&[(0.0, 0.5), (0.7, 0.5)])],
		| b'H' => vec![
			p(&[(0.0, 0.0), (0.0, 1.0)]),
			p(&[(1.0, 0.0), (1.0, 1.0)]),
			p(&[(0.0, 0.5), (1.0, 0.5)]),
		],
		| b'J' => vec![p(&[(1.0, 0.0), (1.0, 0.75), (0.75, 1.0), (0.25, 1.0), (0.0, 0.75)])],
		| b'K' => vec![
			p(&[(0.0, 0.0), (0.0, 1.0)]),
			p(&[(1.0, 0.0), (0.0, 0.62)]),
			p(&[(0.3, 0.42), (1.0, 1.0)]),
		],
		| b'L' => vec![p(&[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0)])],
		| b'M' => vec![p(&[(0.0, 1.0), (0.0, 0.0), (0.5, 0.6), (1.0, 0.0), (1.0, 1.0)])],
		| b'N' => vec![p(&[(0.0, 1.0), (0.0, 0.0), (1.0, 1.0), (1.0, 0.0)])],
		| b'P' => vec![p(&P_BOWL)],
		| b'R' => vec![p(&P_BOWL), p(&[(0.45, 0.55), (1.0, 1.0)])],
		| b'T' => vec![p(&[(0.0, 0.0), (1.0, 0.0)]), p(&[(0.5, 0.0), (0.5, 1.0)])],
		| b'U' => vec![p(&[(0.0, 0.0), (0.0, 0.75), (0.25, 1.0), (0.75, 1.0), (1.0, 0.75), (1.0, 0.0)])],
		| b'V' => vec![p(&[(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)])],
		| b'W' => vec![p(&[(0.0, 0.0), (0.25, 1.0), (0.5, 0.4), (0.75, 1.0), (1.0, 0.0)])],
		| b'X' => vec![p(&[(0.0, 0.0), (1.0, 1.0)]), p(&[(1.0, 0.0), (0.0, 1.0)])],
		| b'Y' => vec![p(&[(0.0, 0.0), (0.5, 0.5), (1.0, 0.0)]), p(&[(0.5, 0.5), (0.5, 1.0)])],
		| b'3' => vec![
			p(&[(0.0, 0.1), (0.3, 0.0), (0.7, 0.0), (1.0, 0.2), (1.0, 0.35), (0.7, 0.5), (0.35, 0.5)]),
			p(&[(0.7, 0.5), (1.0, 0.65), (1.0, 0.8), (0.7, 1.0), (0.3, 1.0), (0.0, 0.9)]),
		],
		| b'4' => vec![p(&[(0.75, 1.0), (0.75, 0.0), (0.0, 0.7), (1.0, 0.7)])],
		| b'6' => vec![p(&SIX)],
		| b'7' => vec![p(&[(0.0, 0.0), (1.0, 0.0), (0.4, 1.0)])],
		| b'9' => vec![SIX.iter().map(|&(x, y)| (1.0 - x, 1.0 - y)).collect()],
		| _ => Vec::new(),
	}
}

const P_BOWL: [Point; 7] =
	[(0.0, 1.0), (0.0, 0.0), (0.7, 0.0), (1.0, 0.15), (1.0, 0.4), (0.7, 0.55), (0.0, 0.55)];

const SIX: [Point; 12] = [
	(0.9, 0.05),
	(0.5, 0.0),
	(0.15, 0.2),
	(0.0, 0.6),
	(0.1, 0.9),
	(0.4, 1.0),
	(0.75, 1.0),
	(1.0, 0.8),
	(1.0, 0.6),
	(0.75, 0.45),
	(0.35, 0.45),
	(0.05, 0.6),
];

/// A random answer from [`ALPHABET`].
pub(super) fn answer() -> String {
	let mut rng = rng();
	(0..LENGTH)
		.map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
		.collect()
}

/// Minimal PNG: 4-bit grayscale, one stored (uncompressed) zlib stream.
/// `pixels` holds one level 0..=15 per pixel.
fn png_gray4(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
	let row = width.div_ceil(2);
	let mut raw = Vec::with_capacity((row + 1) * height);
	for line in pixels.chunks(width) {
		raw.push(0);
		raw.extend(
			line.chunks(2)
				.map(|pair| (pair[0] << 4) | pair.get(1).copied().unwrap_or(0)),
		);
	}

	let mut zlib = vec![0x78, 0x01];
	let blocks: Vec<&[u8]> = raw.chunks(0xFFFF).collect();
	for (i, block) in blocks.iter().enumerate() {
		let len = u16::try_from(block.len()).expect("chunked to fit");
		zlib.push(u8::from(i + 1 == blocks.len()));
		zlib.extend(len.to_le_bytes());
		zlib.extend((!len).to_le_bytes());
		zlib.extend_from_slice(block);
	}
	zlib.extend(adler32(&raw).to_be_bytes());

	let mut header = Vec::with_capacity(13);
	header.extend(u32::try_from(width).expect("small").to_be_bytes());
	header.extend(u32::try_from(height).expect("small").to_be_bytes());
	header.extend([4, 0, 0, 0, 0]);

	let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
	chunk(&mut png, b"IHDR", &header);
	chunk(&mut png, b"IDAT", &zlib);
	chunk(&mut png, b"IEND", &[]);
	png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
	png.extend(u32::try_from(data.len()).expect("small").to_be_bytes());
	let start = png.len();
	png.extend_from_slice(kind);
	png.extend_from_slice(data);
	let crc = crc32(&png[start..]);
	png.extend(crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
	let mut crc = !0_u32;
	for &byte in data {
		crc ^= u32::from(byte);
		for _ in 0..8 {
			crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
		}
	}
	!crc
}

fn adler32(data: &[u8]) -> u32 {
	let (mut a, mut b) = (1_u32, 0_u32);
	for &byte in data {
		a = (a + u32::from(byte)) % 65521;
		b = (b + a) % 65521;
	}
	(b << 16) | a
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_letter_has_strokes() {
		for &c in ALPHABET {
			assert!(!glyph(c).is_empty(), "{}", char::from(c));
		}
	}

	#[test]
	fn draws_a_png() {
		let png = draw("AC3K9W");
		assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
		assert!(png.ends_with(&crc32(b"IEND").to_be_bytes()));
		// 4-bit rows plus a filter byte each, stored uncompressed.
		assert!(png.len() > (WIDTH / 2 + 1) * HEIGHT);
	}

	#[test]
	fn checksums() {
		assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
		assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
	}
}
