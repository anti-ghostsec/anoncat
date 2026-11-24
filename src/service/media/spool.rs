//! Big uploads written to disk as they arrive, instead of held in memory.
//!
//! An upload is read into memory until it passes [`Service::spool_over`]; then
//! what has arrived so far, and the rest as it comes, goes to a file in the
//! spool directory (beside the database, so on the same encrypted disk). The
//! file is streamed from there to the storage providers and removed when the
//! request is done with it, or at the next start if the server stopped first.
//!
//! Spooled uploads are bigger than the video worker takes, so they are stored
//! as they are, as such uploads always were: no metadata cleaning, and the
//! known-bad check compares their SHA-256 only.

use std::{
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
};

use bytes::Bytes;
use futures::Stream;
use sha2::{Digest, Sha256};
use tokio::{
	fs::File,
	io::{AsyncReadExt, AsyncWriteExt},
};
use http::StatusCode;
use ruma::api::error::{ErrorKind, ResourceLimitExceededErrorData};
use tuwunel_core::{Error, Result, implement, utils::sys::storage::available_space, warn};

/// What [`Spooled::head`] keeps of the start, to recognise the file's type.
const HEAD: usize = 64 * 1024;

/// Never kept in memory past this much, whatever the video worker takes.
const SPOOL_FLOOR: usize = 32 * 1024 * 1024;

/// Read back in pieces of this size when it's stored.
const CHUNK: usize = 8 * 1024 * 1024;

const SPOOL_DIR: &str = "upload-spool";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// An upload being written to disk.
pub struct Spool {
	file: File,
	path: PathBuf,
	len: u64,
	hash: Sha256,
	head: Vec<u8>,
}

/// An upload that was written to disk; removed when dropped.
#[derive(Debug)]
pub struct Spooled {
	pub path: PathBuf,
	pub len: u64,
	pub sha256: [u8; 32],
	/// The first bytes, to recognise the file's type.
	pub head: Vec<u8>,
}

impl std::fmt::Debug for Spool {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Spool")
			.field("path", &self.path)
			.field("len", &self.len)
			.finish_non_exhaustive()
	}
}

/// Uploads past this size go to disk.
#[implement(super::Service)]
#[must_use]
pub fn spool_over(&self) -> usize {
	self.services
		.config
		.media_video_thumbnail_max_size
		.max(SPOOL_FLOOR)
}

/// Where uploads are written while they arrive.
#[implement(super::Service)]
#[must_use]
pub fn spool_dir(&self) -> PathBuf { self.services.config.database_path.join(SPOOL_DIR) }

/// Starts writing an upload to disk. Refused while the disk would be left
/// with less than `media_upload_min_free_space` once `expected` bytes are in.
#[implement(super::Service)]
pub async fn spool(&self, expected: u64) -> Result<Spool> {
	let dir = self.spool_dir();
	tokio::fs::create_dir_all(&dir).await?;

	let min_free = self.services.config.media_upload_min_free_space;
	if let Ok(free) = available_space(&dir) {
		if free < expected.saturating_add(min_free) {
			return Err(Error::Request(
				// the spec requires admin_contact; clients use support discovery
				ErrorKind::ResourceLimitExceeded(ResourceLimitExceededErrorData::new(
					String::new(),
				)),
				"The server is short of space for a file this big right now.".into(),
				StatusCode::FORBIDDEN,
			));
		}
	}

	let n = NEXT.fetch_add(1, Ordering::Relaxed);
	let path = dir.join(format!("{}-{n}", std::process::id()));
	let file = File::options()
		.write(true)
		.create_new(true)
		.open(&path)
		.await?;

	Ok(Spool {
		file,
		path,
		len: 0,
		hash: Sha256::new(),
		head: Vec::new(),
	})
}

/// Removes uploads left in the spool by a server that stopped mid-upload.
pub(super) fn clear_spool(config: &tuwunel_core::Config) {
	let dir = spool_dir_of(config);
	let Ok(entries) = std::fs::read_dir(&dir) else {
		return;
	};
	for entry in entries.flatten() {
		if let Err(e) = std::fs::remove_file(entry.path()) {
			warn!("Couldn't remove a left-over upload from {}: {e}", dir.display());
		}
	}
}

fn spool_dir_of(config: &tuwunel_core::Config) -> PathBuf {
	config.database_path.join(SPOOL_DIR)
}

impl Spool {
	pub async fn write(&mut self, data: &[u8]) -> Result {
		if self.head.len() < HEAD {
			let take = data.len().min(HEAD - self.head.len());
			self.head.extend_from_slice(&data[..take]);
		}
		self.hash.update(data);
		self.file.write_all(data).await?;
		self.len = self.len.saturating_add(data.len() as u64);
		Ok(())
	}

	#[must_use]
	pub fn len(&self) -> u64 { self.len }

	#[must_use]
	pub fn is_empty(&self) -> bool { self.len == 0 }

	pub async fn finish(mut self) -> Result<Spooled> {
		self.file.flush().await?;
		self.file.sync_data().await?;
		let path = std::mem::take(&mut self.path);
		let spooled = Spooled {
			path,
			len: self.len,
			sha256: std::mem::take(&mut self.hash).finalize().into(),
			head: std::mem::take(&mut self.head),
		};
		Ok(spooled)
	}
}

impl Drop for Spool {
	fn drop(&mut self) {
		// Not finished: the upload failed or was refused.
		if !self.path.as_os_str().is_empty() {
			_ = std::fs::remove_file(&self.path);
		}
	}
}

impl Drop for Spooled {
	fn drop(&mut self) { _ = std::fs::remove_file(&self.path); }
}

impl Spooled {
	/// The file in pieces of [`CHUNK`] (the last may be shorter), for a
	/// storage provider: S3 refuses multipart pieces under 5 MiB.
	pub async fn chunks(&self) -> Result<impl Stream<Item = Result<Bytes>> + Send + use<>> {
		let file = File::open(&self.path).await?;
		Ok(futures::stream::unfold(Some(file), async |file| {
			let mut file = file?;
			let mut buf = Vec::with_capacity(CHUNK);
			while buf.len() < CHUNK {
				let mut piece = (&mut file).take((CHUNK - buf.len()) as u64);
				match piece.read_buf(&mut buf).await {
					| Ok(0) => break,
					| Ok(_) => {},
					| Err(e) => return Some((Err(e.into()), None)),
				}
			}
			(!buf.is_empty()).then(|| (Ok(Bytes::from(buf)), Some(file)))
		}))
	}

	#[must_use]
	pub fn path(&self) -> &Path { &self.path }
}

#[cfg(test)]
mod tests {
	use futures::TryStreamExt;

	use super::*;

	/// Read back in whole pieces (S3 refuses short ones but the last), with
	/// the same bytes and hash as written, and gone from disk when dropped.
	#[tokio::test]
	async fn spool_round_trip() {
		let dir = std::env::temp_dir().join(format!("anoncat-spool-test-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("upload");
		let file = File::options()
			.write(true)
			.create_new(true)
			.open(&path)
			.await
			.unwrap();
		let mut spool = Spool { file, path: path.clone(), len: 0, hash: Sha256::new(), head: Vec::new() };

		let data: Vec<u8> = (0..(CHUNK * 2 + 12345)).map(|i| (i % 251) as u8).collect();
		for piece in data.chunks(100_000) {
			spool.write(piece).await.unwrap();
		}
		let spooled = spool.finish().await.unwrap();

		assert_eq!(spooled.len, data.len() as u64);
		assert_eq!(spooled.sha256, <[u8; 32]>::from(Sha256::digest(&data)));
		assert_eq!(spooled.head, data[..HEAD]);

		let pieces: Vec<Bytes> = spooled.chunks().await.unwrap().try_collect().await.unwrap();
		assert_eq!(pieces.iter().map(Bytes::len).collect::<Vec<_>>(), [CHUNK, CHUNK, 12345]);
		assert_eq!(pieces.concat(), data);

		drop(spooled);
		assert!(!path.exists());
		std::fs::remove_dir_all(&dir).ok();
	}

	#[tokio::test]
	async fn unfinished_spool_is_removed() {
		let dir = std::env::temp_dir().join(format!("anoncat-spool-test2-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("upload");
		let file = File::options()
			.write(true)
			.create_new(true)
			.open(&path)
			.await
			.unwrap();
		let mut spool = Spool { file, path: path.clone(), len: 0, hash: Sha256::new(), head: Vec::new() };
		spool.write(b"partial").await.unwrap();
		drop(spool);
		assert!(!path.exists());
		std::fs::remove_dir_all(&dir).ok();
	}
}
