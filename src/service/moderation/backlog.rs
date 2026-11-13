//! The AI's pass over media already stored from other servers: what was
//! fetched before checking arrivals was turned on, or while it was off.
//! Pictures and videos from one server (or all of them) are checked one at a
//! time in the background, skipping what's swapped, hidden, or from a
//! blocked server; what the AI flags lands in Reports like any other check.

use std::sync::Arc;

use tuwunel_core::{Err, Result, debug, implement, utils::time::now_millis};

use super::{Image, Service, badfiles::BadScan};

#[implement(Service)]
#[must_use]
pub fn backlog_status(&self) -> BadScan { *self.backlog.read().expect("locked") }

/// Starts the pass; `server` limits it to that server's media.
#[implement(Service)]
pub fn start_backlog(self: &Arc<Self>, server: Option<String>) -> Result {
	let settings = self.settings();
	if !settings.enabled || !settings.check_images {
		return Err!(Request(Forbidden(
			"Turn on AI moderation and image checks in Settings first."
		)));
	}
	if self.key().is_none() {
		return Err!(Request(Forbidden("No Venice API key is saved.")));
	}
	{
		let mut scan = self.backlog.write().expect("locked");
		if scan.running {
			return Err!(Request(Forbidden("A check is already running.")));
		}
		*scan = BadScan { running: true, ..BadScan::default() };
	}

	let this = Arc::clone(self);
	self.services.server.runtime().spawn(async move {
		this.backlog(server.as_deref()).await;
		let mut scan = this.backlog.write().expect("locked");
		scan.running = false;
		scan.finished_ts = Some(now_millis());
	});

	Ok(())
}

#[implement(Service)]
async fn backlog(&self, server: Option<&str>) {
	let media = &self.services.media;
	let items: Vec<_> = media
		.cached_remote()
		.await
		.into_iter()
		.filter(|item| server.is_none_or(|s| item.server.as_str() == s))
		.filter(|item| {
			item.media_type
				.as_deref()
				.is_some_and(|t| t.starts_with("image/") || t.starts_with("video/"))
		})
		.filter(|item| !self.server_blocked(&item.server))
		.collect();
	self.backlog.write().expect("locked").total = items.len();

	for item in items {
		if self.services.server.is_stopping() || !self.settings().enabled {
			return;
		}
		let mxc = item.mxc.to_string();
		let skip = item
			.mxc
			.parts()
			.is_ok_and(|parts| media.is_hidden(&parts) || media.is_swapped(&parts));
		if !skip && !self.checked_upload(&mxc)
			&& let Some(key) = self.key()
		{
			let image = Image {
				item: None,
				uploader: item.server.to_string(),
				origin: "Already stored from another server",
				mxc,
				mime: item.media_type.clone().unwrap_or_default(),
				own: true,
			};
			let settings = self.settings();
			match self.check_image(&settings, &key, &image).await {
				| Ok(true) => {
					let mut scan = self.backlog.write().expect("locked");
					scan.found = scan.found.saturating_add(1);
				},
				| Ok(false) => {},
				| Err(e) => debug!("Couldn't check stored media: {e}"),
			}
		}
		let mut scan = self.backlog.write().expect("locked");
		scan.checked = scan.checked.saturating_add(1);
	}
}
