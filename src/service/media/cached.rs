//! Media from other servers kept here after someone on this server viewed it:
//! listed for the panel, deleted one by one or a whole server at a time.

use std::collections::BTreeMap;

use futures::StreamExt;
use ruma::{Mxc, OwnedMxcUri, OwnedServerName};
use tuwunel_core::{
	Result, implement,
	utils::{self, stream::{BroadbandExt, IterStream}},
};

use super::{Metadata, mtime_millis};

/// One cached item: the file and its thumbnails together.
#[derive(Clone, Debug)]
pub struct CachedMedia {
	pub mxc: OwnedMxcUri,
	pub server: OwnedServerName,
	pub media_type: Option<String>,
	pub bytes: u64,
	/// When it was fetched (the newest of its files).
	pub fetched_ts: u64,
}

/// Every cached item from another server, biggest first.
#[implement(super::Service)]
pub async fn cached_remote(&self) -> Vec<CachedMedia> {
	let ours = self.services.globals.server_name();

	// One key per stored file: the original and each thumbnail size.
	let files: Vec<(OwnedMxcUri, u64, u64)> = self
		.db
		.get_all_media_keys()
		.await
		.into_iter()
		.stream()
		.broad_filter_map(async |key| {
			let mxc = key.split(|&b| b == 0xFF).next()?;
			let mxc = OwnedMxcUri::from(utils::string_from_bytes(mxc).ok()?);
			if !mxc.is_valid() || mxc.server_name().ok()? == ours {
				return None;
			}

			let object = self.head_meta(&key).await?;
			Some((mxc, object.size, mtime_millis(&object)))
		})
		.collect()
		.await;

	let mut items: BTreeMap<OwnedMxcUri, (u64, u64)> = BTreeMap::new();
	for (mxc, bytes, ts) in files {
		let item = items.entry(mxc).or_default();
		item.0 = item.0.saturating_add(bytes);
		item.1 = item.1.max(ts);
	}

	let mut cached: Vec<CachedMedia> = items
		.into_iter()
		.stream()
		.broad_filter_map(async |(mxc, (bytes, fetched_ts))| {
			let parts = mxc.parts().ok()?;
			let media_type = self
				.get_metadata(&parts)
				.await
				.and_then(|Metadata { content_type, .. }| content_type);

			Some(CachedMedia {
				server: parts.server_name.to_owned(),
				mxc,
				media_type,
				bytes,
				fetched_ts,
			})
		})
		.collect()
		.await;

	cached.sort_by(|a, b| b.bytes.cmp(&a.bytes));
	cached
}

/// Deletes this server's copy of one item from another server. Someone
/// viewing it again fetches it again, unless its server is blocked.
#[implement(super::Service)]
pub async fn delete_cached(&self, mxc: &Mxc<'_>) -> Result {
	if self.services.globals.server_is_ours(mxc.server_name) {
		return tuwunel_core::Err!(Request(InvalidParam("That was uploaded here, not cached.")));
	}

	self.delete(mxc).await
}

/// Deletes everything cached from one server; returns how many items and bytes.
#[implement(super::Service)]
pub async fn delete_cached_from(&self, server: &str) -> Result<(usize, u64)> {
	let mut items = 0_usize;
	let mut bytes = 0_u64;
	for item in self.cached_remote().await {
		if item.server.as_str() != server {
			continue;
		}

		let Ok(parts) = item.mxc.parts() else {
			continue;
		};

		if self.delete(&parts).await.is_ok() {
			items = items.saturating_add(1);
			bytes = bytes.saturating_add(item.bytes);
		}
	}

	Ok((items, bytes))
}
