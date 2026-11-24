//! Old uploads copied to a second, bigger store (`media_archive_provider`),
//! such as the home PC's disk mounted over WireGuard. While the server's own
//! disk has room, the original stays too, so either one can be down without
//! anything going missing. Only when the disk fills up are originals that
//! are safely in the archive removed, so nothing has to be deleted.
//!
//! The archive is read like any other provider: after the ones new uploads
//! go to, so recent media never waits on it. When it can't be reached, only
//! archived files are missing until it's back. Files under
//! `media_archive_min_size` (avatars, thumbnails, stickers) stay where they
//! are: they're opened often and take little room.
//!
//! The archive must hold a file named [`MARKER`] at its top: without it (a
//! mount that isn't mounted, say), nothing is moved, so files never land on
//! the server's own disk under the archive's name.

use std::{sync::Arc, time::Duration};

use futures::TryStreamExt;
use tuwunel_core::{Result, debug_warn, implement, info, utils::time::now_millis, warn};

use super::mtime_millis;
use crate::storage::Provider;

/// Must exist at the archive's top before anything is moved there.
pub const MARKER: &str = ".anoncat-archive";

/// What one run did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Moved {
	/// Copied to the archive this run.
	pub files: usize,
	pub bytes: u64,
	/// Originals removed from the server's disk (already in the archive).
	pub evicted: usize,
	pub evicted_bytes: u64,
}

/// The archive provider, when one is set.
#[implement(super::Service)]
pub(super) fn archive_provider(&self) -> Option<Arc<Provider>> {
	let name = &self.services.config.media_archive_provider;
	if name.is_empty() {
		return None;
	}

	self.services
		.storage
		.provider(name)
		.inspect_err(|e| warn!("media_archive_provider {name:?} isn't a storage provider: {e}"))
		.ok()
		.cloned()
}

/// Whether this provider is the archive.
#[implement(super::Service)]
pub(super) fn is_archive(&self, provider: &Provider) -> bool {
	let name = &self.services.config.media_archive_provider;
	!name.is_empty() && provider.name == *name
}

/// Copies this server's own uploads older than `older_than` to the archive,
/// up to `budget` bytes in one run. With `evict`, originals already safely in
/// the archive are also removed from the server's disk; without it they stay.
#[implement(super::Service)]
pub async fn archive_older_than(&self, older_than: Duration, budget: u64, evict: bool) -> Result<Moved> {
	let mut moved = Moved::default();
	let Some(archive) = self.archive_provider() else {
		return Ok(moved);
	};

	if let Err(e) = archive.head(MARKER).await {
		warn!(
			"Old media wasn't copied to the archive: it isn't reachable, or has no {MARKER} \
			 file at its top ({e})."
		);
		return Ok(moved);
	}

	let config = &self.services.config;
	let min_size = config.media_archive_min_size;
	let cutoff = now_millis().saturating_sub(u64::try_from(older_than.as_millis())?);

	for key in self.db.get_all_media_keys().await {
		if moved.bytes >= budget || !self.services.server.is_running() {
			break;
		}

		let Some(mxc) = key.split(|&b| b == 0xFF).next() else {
			continue;
		};
		let Ok(mxc) = std::str::from_utf8(mxc) else {
			continue;
		};
		let Some(server) = mxc
			.strip_prefix("mxc://")
			.and_then(|rest| rest.split('/').next())
		else {
			continue;
		};
		// Media from other servers is only a copy: cleanup deletes it.
		let Ok(server) = <&ruma::ServerName>::try_from(server) else {
			continue;
		};
		if !self.services.globals.server_is_ours(server) {
			continue;
		}

		let path = self.get_media_name_sha256(&key);
		for provider in self.upload_providers() {
			let Ok(meta) = provider.head(&path).await else {
				continue;
			};
			if meta.size < min_size || mtime_millis(&meta) > cutoff {
				continue;
			}

			let archived = archive
				.head(&path)
				.await
				.is_ok_and(|copy| copy.size == meta.size);

			if !archived {
				if let Err(e) = copy_one(provider, &archive, &path, meta.size).await {
					// Most likely the archive went away mid-run: try next time.
					warn!("Copying old media to the archive stopped: {e}");
					return Ok(moved);
				}
				moved.files = moved.files.saturating_add(1);
				moved.bytes = moved.bytes.saturating_add(meta.size);
			}

			if evict {
				match provider.delete_one(&path).await {
					| Ok(()) => {
						moved.evicted = moved.evicted.saturating_add(1);
						moved.evicted_bytes = moved.evicted_bytes.saturating_add(meta.size);
					},
					| Err(e) => debug_warn!("Archived {path} but couldn't remove the original: {e}"),
				}
			}
		}
	}

	if moved.files > 0 || moved.evicted > 0 {
		info!(
			"Archive: copied {} old media files ({} MiB); removed {} originals already there ({} MiB).",
			moved.files,
			moved.bytes >> 20,
			moved.evicted,
			moved.evicted_bytes >> 20,
		);
	}

	Ok(moved)
}

/// Copies one file to the archive and checks the copy is whole.
async fn copy_one(from: &Arc<Provider>, archive: &Arc<Provider>, path: &str, size: u64) -> Result {
	let body = from
		.load(path)
		.await?
		.into_stream()
		.map_err(Into::into);

	archive
		.put(path, Some(usize::try_from(size).unwrap_or(usize::MAX)), body)
		.await?;

	let copy = archive.head(path).await?;
	if copy.size != size {
		_ = archive.delete_one(path).await;
		return Err(tuwunel_core::err!(
			"the archive's copy of {path} has {} bytes, not {size}",
			copy.size
		));
	}

	Ok(())
}
