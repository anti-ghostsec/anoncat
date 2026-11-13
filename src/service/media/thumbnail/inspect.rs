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
#[cfg(not(feature = "media_thumbnail"))]
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
