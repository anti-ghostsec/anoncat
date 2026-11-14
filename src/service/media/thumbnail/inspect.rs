//! Pictures as an admin, or the moderation's AI, needs to see them: the whole
//! of the file the server holds, never a thumbnail someone else chose.
//!
//! A sender picks the `thumbnail_url` their message shows, and another server
//! picks the thumbnails it serves for its own media, so either can be an
//! innocent crop of something that isn't. A picture far taller or wider than
//! it is broad shrinks to a sliver when fitted whole, and an animation's first
//! frame says nothing about the rest. So the preview is made here, from the
//! file itself, and says how big it is and whether it moves; the AI gets the
//! whole picture, then the long ones in overlapping parts and the animated
//! ones as frames from across their length.

use std::{cmp::max, sync::Arc};
#[cfg(feature = "media_thumbnail")]
use std::io::Cursor;

#[cfg(feature = "media_thumbnail")]
use image::{DynamicImage, ImageFormat};
use tuwunel_core::Err;
#[cfg(feature = "media_thumbnail")]
use tuwunel_core::err;
use tuwunel_core::{Result, implement};

#[cfg(feature = "media_thumbnail")]
use super::encode::source_frames;

/// Longest side of the panel's preview.
#[cfg(feature = "media_thumbnail")]
const PREVIEW_SIDE: u32 = 480;
/// Longest side of each view the AI gets.
#[cfg(feature = "media_thumbnail")]
const VIEW_SIDE: u32 = 1024;
/// Parts a long picture is cut into, and frames taken from an animation.
#[cfg(feature = "media_thumbnail")]
const MAX_PARTS: u32 = 4;
/// A picture this many times longer than broad is also looked at in parts.
const LONG_RATIO: u32 = 2;
/// Frames counted before an animation is taken to be this long.
#[cfg(feature = "media_thumbnail")]
const MAX_FRAMES: usize = 500;

/// What a picture is, as found by decoding it.
pub struct Inspected {
	pub width: u32,
	pub height: u32,
	/// 1 for a still picture.
	pub frames: usize,
	/// PNG images: the panel's one preview, or the AI's views.
	pub views: Vec<Vec<u8>>,
}

impl Inspected {
	/// Far longer one way than the other: fitted whole, it's a sliver.
	#[must_use]
	pub fn long(&self) -> bool {
		let (long, short) = (max(self.width, self.height), self.width.min(self.height).max(1));
		long / short >= LONG_RATIO
	}
}

/// The whole picture, fitted into the panel's preview size.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
pub fn admin_preview(&self, bytes: &[u8]) -> Result<Inspected> {
	let image = self.decode(bytes)?;
	let (width, height) = (image.width(), image.height());
	let frames = self.frame_count(bytes);

	Ok(Inspected {
		width,
		height,
		frames,
		views: vec![png(&fit(&image, PREVIEW_SIDE))?],
	})
}

/// The views the AI judges a picture by: the whole of it; a long one also in
/// overlapping parts along its length; an animated one also as frames spread
/// over it.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
pub fn ai_views(&self, bytes: &[u8]) -> Result<Inspected> {
	let image = self.decode(bytes)?;
	let (width, height) = (image.width(), image.height());
	let frames = self.frame_count(bytes);

	let mut inspected = Inspected { width, height, frames, views: Vec::new() };
	let mut views = vec![png(&fit(&image, VIEW_SIDE))?];

	if inspected.long() {
		for part in parts(&image) {
			views.push(png(&fit(&part, VIEW_SIDE))?);
		}
	}

	if frames > 1 {
		for frame in self.spread_frames(bytes, frames) {
			views.push(png(&fit(&frame, VIEW_SIDE))?);
		}
	}

	inspected.views = views;
	Ok(inspected)
}

/// Frames in the picture, 1 for a still one or one that won't read as a
/// sequence.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
fn frame_count(&self, bytes: &[u8]) -> usize {
	let budget = self.services.config.media_thumbnail_max_pixels;
	source_frames(bytes, budget)
		.map(|(frames, _)| frames.take(MAX_FRAMES).take_while(Result::is_ok).count())
		.unwrap_or(1)
		.max(1)
}

/// Up to [`MAX_PARTS`] frames from across the animation, the first left out
/// (the whole-picture view is it).
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
fn spread_frames(&self, bytes: &[u8], count: usize) -> Vec<DynamicImage> {
	let budget = self.services.config.media_thumbnail_max_pixels;
	let Ok((frames, _)) = source_frames(bytes, budget) else {
		return Vec::new();
	};

	let parts = usize::try_from(MAX_PARTS).unwrap_or(4);
	let wanted: Vec<usize> = (1..=parts)
		.map(|i| i.saturating_mul(count.saturating_sub(1)) / parts)
		.filter(|&i| i > 0)
		.collect();

	frames
		.take(count)
		.enumerate()
		.filter(|(i, _)| wanted.contains(i))
		.filter_map(|(_, frame)| frame.ok())
		.map(|frame| DynamicImage::ImageRgba8(frame.into_buffer()))
		.collect()
}

/// A long picture cut along its length into overlapping parts no longer than
/// they are broad, or into [`MAX_PARTS`] longer ones.
#[cfg(feature = "media_thumbnail")]
fn parts(image: &DynamicImage) -> Vec<DynamicImage> {
	let (width, height) = (image.width(), image.height());
	let tall = height > width;
	let (length, breadth) = if tall { (height, width) } else { (width, height) };

	let count = length.div_ceil(breadth.max(1)).clamp(2, MAX_PARTS);
	// Each part overlaps the next by a fifth, so nothing sits only on a cut.
	let size = (length / count).saturating_add(length / count / 5).min(length);
	let step = length.saturating_sub(size) / count.saturating_sub(1).max(1);

	(0..count)
		.map(|i| {
			let start = step.saturating_mul(i).min(length.saturating_sub(size));
			if tall {
				image.crop_imm(0, start, width, size)
			} else {
				image.crop_imm(start, 0, size, height)
			}
		})
		.collect()
}

/// Scaled down to fit a square of `side`, whole; never enlarged.
#[cfg(feature = "media_thumbnail")]
fn fit(image: &DynamicImage, side: u32) -> DynamicImage {
	if image.width() <= side && image.height() <= side {
		return image.clone();
	}

	image.thumbnail(side, side)
}

#[cfg(feature = "media_thumbnail")]
fn png(image: &DynamicImage) -> Result<Vec<u8>> {
	let mut out = Vec::new();
	image
		.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
		.map_err(|e| err!("Couldn't encode a preview: {e}"))?;

	Ok(out)
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
pub fn admin_preview(&self, _bytes: &[u8]) -> Result<Inspected> {
	Err!("This build can't read pictures.")
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
pub fn ai_views(&self, _bytes: &[u8]) -> Result<Inspected> {
	Err!("This build can't read pictures.")
}

/// [`admin_preview`](super::super::Service::admin_preview) off the async
/// threads: decoding a big picture takes a while.
#[implement(super::super::Service)]
pub async fn admin_preview_owned(self: &Arc<Self>, bytes: Vec<u8>) -> Result<Inspected> {
	let this = Arc::clone(self);
	tokio::task::spawn_blocking(move || this.admin_preview(&bytes))
		.await
		.map_err(|e| tuwunel_core::err!("preview failed: {e}"))?
}

/// [`ai_views`](super::super::Service::ai_views), off the async threads.
#[implement(super::super::Service)]
pub async fn ai_views_owned(self: &Arc<Self>, bytes: Vec<u8>) -> Result<Inspected> {
	let this = Arc::clone(self);
	tokio::task::spawn_blocking(move || this.ai_views(&bytes))
		.await
		.map_err(|e| tuwunel_core::err!("check failed: {e}"))?
}

/// What a known-bad file is recognised by. The SHA-256 matches only the very
/// same file; the PDQ hash (Meta's open perceptual hash, the kind shared
/// between services) matches the same picture resized, re-encoded or lightly
/// edited. Neither can be turned back into the picture.
#[derive(Clone, Copy, Debug)]
pub struct Fingerprint {
	pub sha256: [u8; 32],
	/// With its quality, 0 to 100; none when it isn't a picture.
	pub pdq: Option<([u8; 32], u8)>,
}

#[implement(super::super::Service)]
#[must_use]
pub fn fingerprint(&self, bytes: &[u8]) -> Fingerprint {
	use sha2::{Digest, Sha256};

	#[cfg(feature = "media_thumbnail")]
	let pdq = self.decode(bytes).ok().map(|image| {
		let hashed = super::pdq::pdq(&image);
		(hashed.hash, hashed.quality)
	});
	#[cfg(not(feature = "media_thumbnail"))]
	let pdq = None;

	Fingerprint { sha256: Sha256::digest(bytes).into(), pdq }
}

/// The PDQ hashes of a video's frames, with their quality: the six stills of
/// its preview sheet, spread over its whole length. A copy re-encoded,
/// resized or re-muxed gives much the same frames at the same points, so it
/// is recognised where its SHA-256 isn't. Empty for anything that isn't a
/// video, or that the video worker can't read; made in the same single slot
/// as every other video job, so it adds no load beside them.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
pub async fn video_frame_prints(&self, content: &[u8]) -> Vec<([u8; 32], u8)> {
	if !looks_like_video(content) {
		return Vec::new();
	}
	let Ok(sheet) = self.video_sheet(content).await else {
		return Vec::new();
	};
	let Ok(sheet) = self.decode(&sheet) else {
		return Vec::new();
	};

	sheet_tiles(&sheet)
		.iter()
		.map(|frame| {
			let hashed = super::pdq::pdq(frame);
			(hashed.hash, hashed.quality)
		})
		.collect()
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
pub async fn video_frame_prints(&self, _content: &[u8]) -> Vec<([u8; 32], u8)> { Vec::new() }

/// A video's survey (see `video_survey`) as the AI gets it: each sheet of six
/// stills as a JPEG, in order. Empty for anything that isn't a video, or
/// that the video worker can't read.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
pub async fn video_survey_sheets(&self, content: &[u8]) -> Result<Vec<Vec<u8>>> {
	let pngs = self.video_survey(content).await?;
	let sheets: Vec<Vec<u8>> = split_pngs(&pngs)
		.into_iter()
		.filter_map(|png| self.decode(png).ok())
		.filter_map(|sheet| {
			let mut out = Vec::new();
			DynamicImage::ImageRgb8(sheet.to_rgb8())
				.write_to(&mut Cursor::new(&mut out), ImageFormat::Jpeg)
				.ok()
				.map(|()| out)
		})
		.collect();
	if sheets.is_empty() {
		return Err!("no frames came out of it");
	}

	Ok(sheets)
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
pub async fn video_survey_sheets(&self, _content: &[u8]) -> Result<Vec<Vec<u8>>> {
	Err!("This build can't read videos.")
}

/// PNGs written one after another, as ffmpeg's image2pipe does, apart: each
/// ends with its IEND chunk.
#[cfg(feature = "media_thumbnail")]
fn split_pngs(bytes: &[u8]) -> Vec<&[u8]> {
	const END: &[u8] = b"IEND\xaeB`\x82";
	let mut out = Vec::new();
	let mut rest = bytes;
	while let Some(at) = rest.windows(END.len()).position(|w| w == END) {
		let (png, after) = rest.split_at(at.saturating_add(END.len()));
		out.push(png);
		rest = after;
	}

	out
}

/// The six stills of a preview sheet: three across, two down, with four
/// pixels between them (`tile=3x2:padding=4`).
#[cfg(feature = "media_thumbnail")]
fn sheet_tiles(sheet: &DynamicImage) -> Vec<DynamicImage> {
	const PADDING: u32 = 4;
	let width = sheet.width().saturating_sub(2 * PADDING) / 3;
	let height = sheet.height().saturating_sub(PADDING) / 2;
	if width == 0 || height == 0 {
		return Vec::new();
	}

	(0..2_u32)
		.flat_map(|row| (0..3_u32).map(move |column| (row, column)))
		.map(|(row, column)| {
			sheet.crop_imm(column * (width + PADDING), row * (height + PADDING), width, height)
		})
		.collect()
}

/// Whether the bytes start like a video container ffmpeg reads: MP4, MOV and
/// 3GP (but not the AVIF and HEIC pictures that share their box format),
/// Matroska and WebM, AVI, FLV and MPEG-TS. A cheap look, so pictures and
/// other files never cost a video slot.
#[must_use]
pub fn looks_like_video(bytes: &[u8]) -> bool {
	const PICTURE_BRANDS: [&[u8; 4]; 6] = [b"avif", b"avis", b"heic", b"heix", b"mif1", b"msf1"];
	match bytes {
		| [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..] if brand.len() >= 4 =>
			!PICTURE_BRANDS.iter().any(|picture| &brand[..4] == picture.as_slice()),
		| [0x1A, 0x45, 0xDF, 0xA3, ..] | [b'F', b'L', b'V', ..] => true,
		| [b'R', b'I', b'F', b'F', _, _, _, _, b'A', b'V', b'I', b' ', ..] => true,
		| [0x47, ..] => bytes.get(188) == Some(&0x47),
		| _ => false,
	}
}

/// [`fingerprint`](super::super::Service::fingerprint), off the async threads.
#[implement(super::super::Service)]
pub async fn fingerprint_owned(self: &Arc<Self>, bytes: Vec<u8>) -> Result<Fingerprint> {
	let this = Arc::clone(self);
	tokio::task::spawn_blocking(move || this.fingerprint(&bytes))
		.await
		.map_err(|e| tuwunel_core::err!("hashing failed: {e}"))
}

/// PDQ helpers for the known-bad list, which lives outside this module.
#[cfg(feature = "media_thumbnail")]
#[must_use]
pub fn pdq_distance(a: &[u8; 32], b: &[u8; 32]) -> u32 { super::pdq::distance(a, b) }

#[cfg(not(feature = "media_thumbnail"))]
#[must_use]
pub fn pdq_distance(a: &[u8; 32], b: &[u8; 32]) -> u32 {
	a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

#[cfg(all(test, feature = "media_thumbnail"))]
mod frame_tests {
	use super::{looks_like_video, sheet_tiles};

	#[test]
	fn videos_are_told_from_pictures() {
		let boxed = |brand: &[u8; 4]| [&[0, 0, 0, 0x20][..], b"ftyp", brand, &[0; 8]].concat();
		assert!(looks_like_video(&boxed(b"isom")));
		assert!(looks_like_video(&boxed(b"qt  ")));
		assert!(!looks_like_video(&boxed(b"avif")));
		assert!(!looks_like_video(&boxed(b"heic")));
		assert!(looks_like_video(&[0x1A, 0x45, 0xDF, 0xA3, 1, 2]));
		assert!(looks_like_video(b"RIFF\0\0\0\0AVI LIST"));
		assert!(!looks_like_video(b"RIFF\0\0\0\0WEBPVP8 "));
		assert!(!looks_like_video(b"\x89PNG\r\n\x1a\n"));
		assert!(!looks_like_video(&[0x47; 10]));
		let mut ts = vec![0_u8; 400];
		ts[0] = 0x47;
		ts[188] = 0x47;
		assert!(looks_like_video(&ts));
	}

	#[test]
	fn pngs_one_after_another_come_apart() {
		let png = |w| {
			let mut out = Vec::new();
			image::DynamicImage::new_rgb8(w, 2)
				.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
				.unwrap();
			out
		};
		let joined = [png(3), png(5), png(7)].concat();
		let apart = super::split_pngs(&joined);
		assert_eq!(apart.len(), 3);
		assert_eq!(image::load_from_memory(apart[2]).unwrap().width(), 7);
	}

	#[test]
	fn a_sheet_splits_into_its_six_frames() {
		let sheet = image::DynamicImage::new_rgb8(968, 484);
		let tiles = sheet_tiles(&sheet);
		assert_eq!(tiles.len(), 6);
		assert!(tiles.iter().all(|t| t.width() == 320 && t.height() == 240));
	}
}
