use ruma::{RoomId, events::room::message::RoomMessageEventContent};
use tuwunel_core::{Result, implement};

/// Records a notice for the admins, shown in the panel.
#[implement(super::Service)]
pub async fn notify(&self, body: &str) { self.record_notice(body, false); }

/// Records a notice the admins should see soon, shown highlighted in the
/// panel.
#[implement(super::Service)]
pub async fn notify_loud(&self, body: &str) { self.record_notice(body, true); }

/// Records an admin command response or other notice for the panel.
#[implement(super::Service)]
pub async fn notice(&self, body: &str) { self.record_notice(body, false); }

/// Records a notice for the panel.
#[implement(super::Service)]
pub async fn send_text(&self, body: &str) { self.record_notice(body, false); }

/// Records a report for the panel.
#[implement(super::Service)]
pub async fn send_report(&self, body: &str) { self.record_notice(body, true); }

/// Records a message's text for the panel; anoncat has no admin room.
#[implement(super::Service)]
pub async fn send_message(&self, message_content: RoomMessageEventContent) -> Result {
	self.record_notice(message_content.body(), false);
	Ok(())
}

/// Most notices kept; older ones are dropped.
const NOTICES_KEPT: usize = 1000;
/// Notices older than this are deleted: 30 days.
const NOTICES_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// One notice, as stored and as the panel shows it.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Notice {
	pub ts: u64,
	pub loud: bool,
	pub body: String,
}

/// Stores a notice under its time, so they list in order.
#[implement(super::Service)]
pub fn record_notice(&self, body: &str, loud: bool) {
	use std::sync::Mutex;

	// Unique and increasing even when two land in the same millisecond.
	static LAST: Mutex<u64> = Mutex::new(0);
	let now = tuwunel_core::utils::time::now_millis();
	let key = {
		let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		*last = now.max(last.saturating_add(1));
		*last
	};

	// Error text can carry an address; none is ever stored.
	let body = crate::logs::scrub_ips(body);
	let notice = Notice { ts: now, loud, body };
	if let Ok(value) = serde_json::to_vec(&notice) {
		self.notices.insert(&key.to_be_bytes(), value);
	}

	if loud {
		tuwunel_core::warn!("{}", notice.body);
	} else {
		tuwunel_core::info!("{}", notice.body);
	}
}

/// The newest notices, newest first, and drops those past the most kept.
#[implement(super::Service)]
pub async fn notices(&self, limit: usize) -> Vec<Notice> {
	use futures::StreamExt;
	use tuwunel_core::utils::stream::TryIgnore;

	let all: Vec<(Vec<u8>, Notice)> = self
		.notices
		.rev_raw_stream()
		.ignore_err()
		.filter_map(async |(key, value)| {
			serde_json::from_slice(value).ok().map(|n| (key.to_vec(), n))
		})
		.collect()
		.await;

	// Past the most kept, or older than 30 days: notices name accounts and
	// what they did, and mustn't outlive the messages themselves.
	let cutoff = tuwunel_core::utils::time::now_millis().saturating_sub(NOTICES_MAX_AGE_MS);
	let mut kept = Vec::new();
	for (at, (key, notice)) in all.into_iter().enumerate() {
		if at >= NOTICES_KEPT || notice.ts < cutoff {
			self.notices.remove(&key);
		} else {
			kept.push(notice);
		}
	}

	kept.into_iter().take(limit).collect()
}

/// anoncat has no bot account to send messages with.
#[implement(super::Service)]
pub async fn send_to_room(&self, _content: RoomMessageEventContent, _room_id: &RoomId) -> Result {
	tuwunel_core::Err!(Request(Forbidden("This server has no bot account to send messages with.")))
}
