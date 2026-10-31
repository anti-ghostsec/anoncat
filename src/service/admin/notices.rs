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

	let notice = Notice { ts: now, loud, body: body.to_owned() };
	if let Ok(value) = serde_json::to_vec(&notice) {
		self.notices.insert(&key.to_be_bytes(), value);
	}

	if loud {
		tuwunel_core::warn!("{body}");
	} else {
		tuwunel_core::info!("{body}");
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

	for (key, _) in all.iter().skip(NOTICES_KEPT) {
		self.notices.remove(key);
	}

	all.into_iter().take(limit).map(|(_, n)| n).collect()
}

/// anoncat has no bot account to send messages with.
#[implement(super::Service)]
pub async fn send_to_room(&self, _content: RoomMessageEventContent, _room_id: &RoomId) -> Result {
	tuwunel_core::Err!(Request(Forbidden("This server has no bot account to send messages with.")))
}
