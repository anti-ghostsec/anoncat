//! Downloads of big files without holding them in memory. A video player
//! fetches a file a piece at a time (`Range: bytes=…`), often many pieces at
//! once; each piece is read from storage on its own. A download of a whole
//! big file is streamed from storage as it goes out. Either way a large
//! video never has to sit in memory whole.
//!
//! The router sets [`WANTED`] around a download request; the download
//! handler asks [`Service::get_piece`], and the router turns the answer into
//! 206 Partial Content or a streamed body.

use std::sync::Mutex;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream::BoxStream};
use ruma::Mxc;

use super::{Animate, Dim, Media, Metadata, Service};

tokio::task_local! {
	/// The range a download request asked for, and what was answered.
	pub static WANTED: Wanted;
}

pub struct Wanted {
	/// The `Range` header, or `None` for the whole file.
	pub spec: Option<String>,
	pub answer: Mutex<Option<Answer>>,
}

pub enum Answer {
	/// Bytes `start..=end` of a file `total` bytes long.
	Piece { start: u64, end: u64, total: u64 },
	/// The range starts past the end of the file.
	Unsatisfiable { total: u64 },
	/// The whole file, to be sent as it's read.
	Whole { total: u64, body: BoxStream<'static, std::io::Result<Bytes>> },
}

/// Files bigger than this are streamed when downloaded whole.
pub const STREAM_OVER: u64 = 16 * 1024 * 1024;

/// The most sent for one range. An open-ended range ("from here to the
/// end") gets this much and the player asks for the next piece; players
/// expect that.
pub const MAX_PIECE: u64 = 8 * 1024 * 1024;

impl Wanted {
	#[must_use]
	pub fn new(spec: Option<String>) -> Self { Self { spec, answer: Mutex::new(None) } }

	#[must_use]
	pub fn take(&self) -> Option<Answer> { self.answer.lock().expect("not poisoned").take() }
}

impl Service {
	/// The piece of a stored file the current request asked for, read on its
	/// own, or for a big file downloaded whole, an empty body and the file
	/// as a stream in [`WANTED`]. `None` when the file isn't stored here, is
	/// small, or the range isn't one answered: then the whole file is fetched
	/// the usual way.
	pub async fn get_piece(&self, mxc: &Mxc<'_>) -> Option<Media> {
		let spec = WANTED.try_with(|w| w.spec.clone()).ok()?;
		self.check_hidden(mxc).ok()?;

		let Metadata { content_type, content_disposition, key } = self
			.db
			.search_file_metadata(mxc, &Dim::default(), Animate::Allowed)
			.await
			.ok()?;
		let path = self.get_media_name_sha256(&key);

		for provider in self.storage_providers() {
			let Ok(meta) = provider.head(&path).await else {
				continue;
			};
			let total = meta.size;
			let Some(spec) = &spec else {
				if total <= STREAM_OVER {
					return None;
				}
				let body = provider
					.load(&path)
					.await
					.ok()?
					.into_stream()
					.map_err(std::io::Error::other)
					.boxed();
				WANTED.with(|w| {
					*w.answer.lock().expect("not poisoned") = Some(Answer::Whole { total, body });
				});

				return Some(Media { content: Vec::new(), content_type, content_disposition });
			};
			let (content, answer) = match parse(spec, total)? {
				| Ok((start, end)) => {
					let end = end.min(start.saturating_add(MAX_PIECE - 1));
					let bytes = provider
						.get_range(&path, start..end.saturating_add(1))
						.await
						.ok()?;
					(bytes.to_vec(), Answer::Piece { start, end, total })
				},
				| Err(()) => (Vec::new(), Answer::Unsatisfiable { total }),
			};
			WANTED.with(|w| *w.answer.lock().expect("not poisoned") = Some(answer));

			return Some(Media { content, content_type, content_disposition });
		}

		None
	}
}

/// The first and last byte asked for. `None`: not a single byte range we
/// answer, so the whole file goes out. `Err`: a range wholly past the end.
#[must_use]
pub fn parse(range: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
	let spec = range.trim().strip_prefix("bytes=")?.trim();
	if spec.contains(',') {
		return None;
	}
	let (first, last) = spec.split_once('-')?;
	let (first, last) = (first.trim(), last.trim());

	if first.is_empty() {
		// The last N bytes.
		let n: u64 = last.parse().ok()?;
		return Some(if n == 0 || len == 0 {
			Err(())
		} else {
			Ok((len.saturating_sub(n), len - 1))
		});
	}

	let start: u64 = first.parse().ok()?;
	let end = match last {
		| "" => u64::MAX,
		| last => last.parse().ok()?,
	};
	if end < start {
		return None;
	}
	if start >= len {
		return Some(Err(()));
	}
	Some(Ok((start, end.min(len - 1))))
}

#[cfg(test)]
mod tests {
	use super::parse;

	#[test]
	fn ranges() {
		assert_eq!(parse("bytes=0-9", 100), Some(Ok((0, 9))));
		assert_eq!(parse("bytes=90-", 100), Some(Ok((90, 99))));
		assert_eq!(parse("bytes=50-500", 100), Some(Ok((50, 99))));
		assert_eq!(parse("bytes=-10", 100), Some(Ok((90, 99))));
		assert_eq!(parse("bytes=-500", 100), Some(Ok((0, 99))));
		assert_eq!(parse("bytes=100-", 100), Some(Err(())));
		assert_eq!(parse("bytes=-0", 100), Some(Err(())));
		assert_eq!(parse("bytes=0-1,5-6", 100), None);
		assert_eq!(parse("bytes=9-0", 100), None);
		assert_eq!(parse("items=0-9", 100), None);
		assert_eq!(parse("bytes=x-9", 100), None);
	}
}
