//! Keeping this server from being used as free file storage.
//!
//! Tools exist that sign in to any homeserver, cut a file into encrypted
//! pieces of about a megabyte (`chunk_0.enc`, `chunk_1.enc`, ...) and upload
//! them as media without ever posting them in a room. The list of pieces is
//! shared somewhere else, so anyone can put the file back together through
//! this server, and nobody here ever sees what it is.
//!
//! Real attachments get posted: one message per file, even in encrypted rooms,
//! where the server can't read which file a message carries but still sees
//! the message. So an account may upload only as many files as it posted
//! messages over the last day, plus an allowance; pieces named like those
//! tools' are refused outright. Admins aren't limited.
//!
//! To find what got through, every event's mxc:// links are recorded with the
//! room they appeared in (`anoncat_mxc_rooms`), and each local account's posts
//! are counted (`anoncat_sender_posts`). Events from before this existed are
//! read once, in the background, at the first start; the panel can ask again.

use std::{
	collections::{BTreeMap, HashMap, VecDeque},
	sync::{
		Arc, LazyLock,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use futures::StreamExt;
use regex::Regex;
use ruma::{Mxc, OwnedRoomId, OwnedUserId, RoomId, UserId, events::TimelineEventType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use tuwunel_core::{
	Err, Result, implement, info,
	utils::{stream::TryIgnore, time::now_millis},
	warn,
};
use tuwunel_matrix::{Event, PduEvent};

/// Marks that events from before the index existed have been read.
const INDEXED_KEY: &[u8] = b"storage_index:v1";
/// How far back uploads and posts are compared.
const WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Links recorded from one event; real ones carry one or two.
const MXCS_PER_EVENT: usize = 32;
/// How deep into an event's content links are looked for.
const MAX_DEPTH: usize = 6;
/// Uploads beyond posts before an account is listed as suspicious.
const FLAG_SLACK: usize = 20;

/// File-splitting tools' piece names: `chunk_12.enc`, `part0003.bin` and
/// the like.
static PIECE_NAME: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"(?i)^(chunk|part|piece|shard|blob|seg(ment)?)[_\-.]?\d+(\.(enc|bin|dat|part|chunk))?$")
		.expect("valid regex")
});

/// Whether an upload's file name is one of those tools' pieces.
#[must_use]
pub fn piece_name(name: &str) -> bool { PIECE_NAME.is_match(name.trim()) }

/// One account's uploads and posts over the last [`WINDOW`], kept in memory.
#[derive(Default)]
pub(super) struct Recent {
	uploads: VecDeque<Instant>,
	posts: VecDeque<Instant>,
}

impl Recent {
	fn prune(&mut self, now: Instant) {
		for times in [&mut self.uploads, &mut self.posts] {
			while times
				.front()
				.is_some_and(|t| now.duration_since(*t) > WINDOW)
			{
				times.pop_front();
			}
		}
	}
}

/// The background read of past events.
#[derive(Default)]
pub(super) struct Scan {
	running: AtomicBool,
	events: AtomicU64,
}

/// Where the index stands, for the panel.
#[derive(Clone, Copy, Serialize)]
pub struct IndexStatus {
	/// Events from before the index existed have all been read.
	pub indexed: bool,
	pub scanning: bool,
	/// Events read so far by the running (or last) scan.
	pub events_read: u64,
}

/// One local account's uploads, for the panel's storage check.
#[derive(Serialize)]
pub struct Uploader {
	pub user_id: String,
	pub uploads: usize,
	pub bytes: u64,
	/// Uploads whose link was seen in some room. Files posted in encrypted
	/// rooms never are, since their messages can't be read.
	pub in_rooms: usize,
	/// Messages they've posted, encrypted ones included.
	pub posts: u64,
	pub pieces: usize,
	pub piece_bytes: u64,
	pub newest_ts: u64,
	pub flagged: bool,
	pub reasons: Vec<String>,
}

/// Records an event's mxc:// links and counts a local account's post. Runs on
/// every event this server accepts.
#[implement(super::Service)]
pub fn record_event(&self, pdu: &PduEvent) {
	let content = pdu.content().get();
	if content.contains("mxc://") {
		for mxc in mxcs_in(content) {
			self.mxc_rooms
				.insert(&mxc_room_key(&mxc, pdu.room_id()), []);
		}
	}

	if is_post(pdu.kind()) && self.services.globals.user_is_local(pdu.sender()) {
		self.add_posts(pdu.sender(), 1);
		self.recent
			.lock()
			.expect("locked")
			.entry(pdu.sender().to_owned())
			.or_default()
			.posts
			.push_back(Instant::now());
	}
}

/// Refuses an upload that would make this server someone's file storage, and
/// counts it otherwise.
#[implement(super::Service)]
pub async fn check_upload(&self, user: &UserId, filename: Option<&str>) -> Result {
	if !self.guards(user).await {
		return Ok(());
	}

	check_name(filename)?;

	let allowance = usize::try_from(self.settings().upload_allowance).unwrap_or(usize::MAX);
	let now = Instant::now();
	let mut recent = self.recent.lock().expect("locked");
	let mine = recent.entry(user.to_owned()).or_default();
	mine.prune(now);
	if mine.uploads.len() >= mine.posts.len().saturating_add(allowance) {
		return Err!(Request(Forbidden(
			"You've uploaded many files today without posting them. Post what you've \
			 uploaded in a room before uploading more."
		)));
	}

	mine.uploads.push_back(now);

	Ok(())
}

/// Refuses only a split-file piece: for content sent to a link made earlier,
/// which was counted when it was made.
#[implement(super::Service)]
pub async fn check_upload_name(&self, user: &UserId, filename: Option<&str>) -> Result {
	if self.guards(user).await {
		check_name(filename)?;
	}

	Ok(())
}

/// Whether the upload limits apply to this account.
#[implement(super::Service)]
async fn guards(&self, user: &UserId) -> bool {
	self.settings().upload_guard && !self.services.admin.user_is_admin(user).await
}

fn check_name(filename: Option<&str>) -> Result {
	if filename.is_some_and(piece_name) {
		return Err!(Request(Forbidden(
			"This server doesn't store pieces of split-up files. Send files whole, in a room."
		)));
	}

	Ok(())
}

/// Rooms an mxc:// link has been seen in.
#[implement(super::Service)]
pub async fn rooms_of(&self, mxc: &str) -> Vec<OwnedRoomId> {
	let prefix = mxc_prefix(mxc);
	self.mxc_rooms
		.raw_keys_prefix(&prefix)
		.ignore_err()
		.filter_map(async |key| {
			let room = std::str::from_utf8(key.get(prefix.len()..)?).ok()?;
			RoomId::parse(room).ok()
		})
		.collect()
		.await
}

/// Whether an mxc:// link has been seen in any room.
#[implement(super::Service)]
pub async fn in_any_room(&self, mxc: &str) -> bool {
	let prefix = mxc_prefix(mxc);
	let keys = self.mxc_rooms.raw_keys_prefix(&prefix).ignore_err();
	futures::pin_mut!(keys);
	keys.next().await.is_some()
}

/// The thumbnails the messages carrying a file show with it, from rooms that
/// aren't encrypted. A video's still, or a picture's small copy, is a file of
/// its own, and stays when only the file is swapped.
#[implement(super::Service)]
pub async fn thumbnails_of(&self, mxc: &str) -> Vec<String> {
	let mut found: Vec<String> = Vec::new();
	for room in self.rooms_of(mxc).await {
		let posts: Vec<_> = self
			.services
			.timeline
			.pdus_rev(None, &room, None)
			.ignore_err()
			.take(THUMBS_EVENTS_READ)
			.filter(|(_, pdu)| std::future::ready(pdu.content().get().contains(mxc)))
			.take(THUMBS_POSTS)
			.collect()
			.await;
		for (_, pdu) in posts {
			let Ok(content) = serde_json::from_str::<Value>(pdu.content().get()) else {
				continue;
			};
			// Only a message that is this file: one that merely uses it as
			// its own thumbnail doesn't make its file a thumbnail.
			if content.get("url").and_then(Value::as_str) != Some(mxc) {
				continue;
			}
			let info = content.get("info");
			let thumbs = [
				info.and_then(|i| i.get("thumbnail_url")),
				info.and_then(|i| i.get("thumbnail_file"))
					.and_then(|f| f.get("url")),
			];
			for thumb in thumbs.into_iter().flatten().filter_map(Value::as_str) {
				if thumb != mxc
					&& Mxc::try_from(thumb).is_ok()
					&& !found.iter().any(|f| f == thumb)
				{
					found.push(thumb.to_owned());
				}
			}
		}
	}

	found
}

/// Messages a local account has posted, as far as this server knows.
#[implement(super::Service)]
pub async fn posts_by(&self, user: &UserId) -> u64 {
	self.sender_posts
		.get(user.as_bytes())
		.await
		.ok()
		.and_then(|v| v.as_ref().try_into().ok())
		.map_or(0, u64::from_be_bytes)
}

#[implement(super::Service)]
fn add_posts(&self, user: &UserId, n: u64) {
	// One lock for every count, so two posts at once both land.
	let _guard = self.posts_lock.lock().expect("locked");
	let old = self
		.sender_posts
		.get_blocking(user.as_bytes())
		.ok()
		.and_then(|v| v.as_ref().try_into().ok())
		.map_or(0, u64::from_be_bytes);
	self.sender_posts
		.insert(user.as_bytes(), old.saturating_add(n).to_be_bytes());
}

#[implement(super::Service)]
pub async fn index_status(&self) -> IndexStatus {
	IndexStatus {
		indexed: self.settings_map.get(INDEXED_KEY).await.is_ok(),
		scanning: self.scan.running.load(Ordering::Relaxed),
		events_read: self.scan.events.load(Ordering::Relaxed),
	}
}

/// Reads past events once, at the first start after the index was added.
#[implement(super::Service)]
pub(super) async fn index_if_new(self: &Arc<Self>) {
	if self.settings_map.get(INDEXED_KEY).await.is_err() {
		self.start_index_scan();
	}
}

/// Reads every stored event into the index, in the background. Does nothing
/// while a read is running already.
#[implement(super::Service)]
pub fn start_index_scan(self: &Arc<Self>) {
	if self.scan.running.swap(true, Ordering::Relaxed) {
		return;
	}

	let this = Arc::clone(self);
	self.services.server.runtime().spawn(async move {
		this.scan.events.store(0, Ordering::Relaxed);
		info!("Indexing where media appears in rooms...");
		let Some(posts) = this.index_scan().await else {
			this.scan.running.store(false, Ordering::Relaxed);
			return;
		};
		this.sender_posts.clear().await;
		for (user, n) in posts {
			this.sender_posts
				.insert(user.as_bytes(), n.to_be_bytes());
		}
		this.settings_map
			.insert(INDEXED_KEY, now_millis().to_be_bytes());
		info!("Media index done: {} events read", this.scan.events.load(Ordering::Relaxed));
		this.scan.running.store(false, Ordering::Relaxed);
	});
}

/// Records every stored event's links, and returns each local account's
/// posts; nothing when the server began shutting down before the end.
#[implement(super::Service)]
async fn index_scan(&self) -> Option<BTreeMap<OwnedUserId, u64>> {
	let mut posts: HashMap<OwnedUserId, u64> = HashMap::new();
	let events = self.services.timeline.pdus_raw().ignore_err();
	futures::pin_mut!(events);
	loop {
		// Each event is done with before the next await; they borrow the cursor.
		match events.next().await {
			| Some(value) => self.index_row(value, &mut posts),
			| None => break,
		}
		let read = self.scan.events.fetch_add(1, Ordering::Relaxed);
		if read % 1024 == 0 {
			tokio::task::yield_now().await;
			if self.services.server.is_stopping() {
				warn!("Media index stopped for shutdown; it runs again at the next start.");
				return None;
			}
		}
	}

	Some(posts.into_iter().collect())
}

#[implement(super::Service)]
fn index_row(&self, value: &[u8], posts: &mut HashMap<OwnedUserId, u64>) {
	#[derive(Deserialize)]
	struct Row<'a> {
		#[serde(borrow)]
		room_id: Option<&'a str>,
		#[serde(borrow)]
		sender: &'a str,
		#[serde(rename = "type", borrow)]
		kind: &'a str,
		#[serde(borrow)]
		content: Option<&'a RawValue>,
	}

	let Ok(row) = serde_json::from_slice::<Row<'_>>(value) else {
		return;
	};
	if let Some(room) = row.room_id.and_then(|r| RoomId::parse(r).ok())
		&& let Some(content) = row.content
		&& content.get().contains("mxc://")
	{
		for mxc in mxcs_in(content.get()) {
			self.mxc_rooms
				.insert(&mxc_room_key(&mxc, &room), []);
		}
	}
	if is_post(&row.kind.into())
		&& let Ok(user) = UserId::parse(row.sender)
		&& self.services.globals.user_is_local(&user)
	{
		let n = posts.entry(user).or_default();
		*n = n.saturating_add(1);
	}
}

/// Every local account with uploads, the suspicious ones first.
#[implement(super::Service)]
pub async fn uploaders(&self) -> Vec<Uploader> {
	let by_user = self.services.media.uploads_by_user().await;
	let mut out = Vec::with_capacity(by_user.len());
	for (user, entries) in by_user {
		let mut row = Uploader {
			user_id: user.to_string(),
			uploads: entries.len(),
			bytes: 0,
			in_rooms: 0,
			posts: self.posts_by(&user).await,
			pieces: 0,
			piece_bytes: 0,
			newest_ts: 0,
			flagged: false,
			reasons: Vec::new(),
		};
		for entry in &entries {
			let bytes = entry.media_length.unwrap_or(0);
			row.bytes = row.bytes.saturating_add(bytes);
			row.newest_ts = row.newest_ts.max(entry.created_ts);
			if self.in_any_room(entry.mxc.as_str()).await {
				row.in_rooms = row.in_rooms.saturating_add(1);
			}
			if entry.upload_name.as_deref().is_some_and(piece_name) {
				row.pieces = row.pieces.saturating_add(1);
				row.piece_bytes = row.piece_bytes.saturating_add(bytes);
			}
		}

		if row.pieces > 0 {
			row.reasons.push(if row.pieces == 1 {
				"1 upload is named like a piece of a split-up file (chunk_0.enc and so on)".to_owned()
			} else {
				format!(
					"{} uploads are named like pieces of a split-up file (chunk_0.enc and so on)",
					row.pieces
				)
			});
		}
		let posts = usize::try_from(row.posts).unwrap_or(usize::MAX);
		if row.uploads > posts.saturating_mul(2).saturating_add(FLAG_SLACK) {
			row.reasons.push(format!(
				"{} uploads but only {} messages posted",
				row.uploads, row.posts
			));
		}
		row.flagged = !row.reasons.is_empty();
		out.push(row);
	}

	out.sort_by(|a, b| b.flagged.cmp(&a.flagged).then(b.bytes.cmp(&a.bytes)));
	out
}

/// Deletes a local account's uploads: only the split-file pieces, or all.
/// Returns how many and how many bytes.
#[implement(super::Service)]
pub async fn delete_uploads(&self, user: &UserId, only_pieces: bool) -> Result<(usize, u64)> {
	if !self.services.globals.user_is_local(user) {
		return Err!(Request(InvalidParam("{user} isn't an account on this server.")));
	}

	let entries = self.services.media.user_media(user).await?;
	let (mut items, mut bytes) = (0_usize, 0_u64);
	for entry in entries {
		if only_pieces && !entry.upload_name.as_deref().is_some_and(piece_name) {
			continue;
		}
		let Ok(mxc) = <Mxc<'_>>::try_from(entry.mxc.as_str()) else {
			continue;
		};
		match self.services.media.delete(&mxc).await {
			| Ok(()) => {
				items = items.saturating_add(1);
				bytes = bytes.saturating_add(entry.media_length.unwrap_or(0));
			},
			| Err(e) => warn!("Couldn't delete {mxc}: {e}"),
		}
	}

	Ok((items, bytes))
}

/// Messages that carry a file, or could: everything a person posts.
fn is_post(kind: &TimelineEventType) -> bool {
	matches!(
		kind,
		TimelineEventType::RoomMessage
			| TimelineEventType::RoomEncrypted
			| TimelineEventType::Sticker
	)
}

/// Messages read back per room looking for the ones that carry a file.
const THUMBS_EVENTS_READ: usize = 50_000;
/// Messages carrying the file looked at per room.
const THUMBS_POSTS: usize = 20;

fn mxc_prefix(mxc: &str) -> Vec<u8> {
	let mut key = mxc.as_bytes().to_vec();
	key.push(0xFF);
	key
}

fn mxc_room_key(mxc: &str, room: &RoomId) -> Vec<u8> {
	let mut key = mxc_prefix(mxc);
	key.extend_from_slice(room.as_bytes());
	key
}

/// The valid mxc:// links anywhere in an event's content.
fn mxcs_in(content: &str) -> Vec<String> {
	fn walk(value: &Value, depth: usize, out: &mut Vec<String>) {
		if out.len() >= MXCS_PER_EVENT || depth > MAX_DEPTH {
			return;
		}
		match value {
			| Value::String(s) if s.starts_with("mxc://") =>
				if Mxc::try_from(s.as_str()).is_ok() && !out.contains(s) {
					out.push(s.clone());
				},
			| Value::Array(items) => items
				.iter()
				.for_each(|v| walk(v, depth.saturating_add(1), out)),
			| Value::Object(map) => map
				.values()
				.for_each(|v| walk(v, depth.saturating_add(1), out)),
			| _ => {},
		}
	}

	let mut out = Vec::new();
	if let Ok(value) = serde_json::from_str::<Value>(content) {
		walk(&value, 0, &mut out);
	}

	out
}

#[cfg(test)]
mod tests {
	use super::{mxcs_in, piece_name};

	#[test]
	fn pieces() {
		for name in ["chunk_0.enc", "chunk_1234.enc", "CHUNK_7.ENC", "part0003.bin", "piece-12"] {
			assert!(piece_name(name), "{name}");
		}
		for name in ["holiday.jpg", "chunk.enc", "image.png", "partner.pdf", "chunky_5.mp4"] {
			assert!(!piece_name(name), "{name}");
		}
	}

	#[test]
	fn links() {
		let content = r#"{"msgtype":"m.image","url":"mxc://a.org/abc","info":{"thumbnail_url":"mxc://a.org/def"},"body":"mxc://not valid"}"#;
		let mut found = mxcs_in(content);
		found.sort();
		assert_eq!(found, ["mxc://a.org/abc", "mxc://a.org/def"]);
	}
}
