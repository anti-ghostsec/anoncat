//! Known bad files: fingerprints of media an admin swapped away, or imported
//! from a list someone shares, so the same picture is never taken in again.
//!
//! A file is recognised two ways. Its SHA-256 matches only that very file.
//! Its PDQ hash (Meta's open perceptual hash, the format hash lists are
//! commonly shared in) matches the same picture resized, re-encoded or
//! lightly edited, within a Hamming distance of [`PDQ_MATCH`]. Neither is
//! the picture or can be turned back into it: a hash is 32 bytes, the same
//! for countless images, which is why such lists are what services share in
//! place of the files.
//!
//! A video is recognised by its frames too: the PDQ hashes of six stills
//! from across its length, stored as `pdq-frame`. One frame isn't enough to
//! call a video the same, since many share an intro or an app's end screen;
//! [`FRAMES_MATCH`] of them are (or every frame, for a video with fewer good
//! ones). A frame that matches a picture on the list (`pdq`) is enough.
//!
//! A local upload that matches is refused. A file from another server that
//! matches is swapped for the stand-in before anyone here sees it, and
//! reported. Swapping media adds its fingerprints here on its own.
//!
//! The list is kept in the settings map under `badfile:`, so it's in the
//! encrypted database and its backups, and is exported and imported as
//! plain text: one `kind,hash,note` line each.

use std::{
	collections::HashSet,
	sync::{Arc, RwLock},
};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Result, implement,
	utils::{stream::TryIgnore, time::now_millis},
	warn,
};
use crate::media::{Fingerprint, pdq_distance};

use super::Service;

const PREFIX: &[u8] = b"badfile:";
/// Bits two PDQ hashes may differ by and still be the same picture: the
/// threshold PDQ's authors give for few false matches.
const PDQ_MATCH: u32 = 31;
/// PDQ hashes below this quality (flat or tiny pictures) match too much to
/// be trusted, and are neither added nor matched.
const PDQ_MIN_QUALITY: u8 = 50;
/// Frames of a video that must match frames of one on the list.
const FRAMES_MATCH: usize = 2;
/// Longest note kept.
const NOTE_CHARS: usize = 200;

/// One fingerprint on the list.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BadFile {
	/// "pdq", "pdq-frame" (one frame of a video) or "sha256".
	pub kind: String,
	/// 64 hex digits.
	pub hash: String,
	pub note: String,
	pub added_ts: u64,
	/// "swap", "import" or "admin".
	pub source: String,
}

/// The list as matched, in memory.
#[derive(Default)]
pub(super) struct List {
	sha256: HashSet<[u8; 32]>,
	pdq: Vec<[u8; 32]>,
	frames: Vec<[u8; 32]>,
}

pub(super) type State = RwLock<List>;

fn hex(bytes: &[u8; 32]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
	let text = text.trim();
	if text.len() != 64 || !text.is_ascii() {
		return None;
	}
	let mut out = [0_u8; 32];
	for (i, byte) in out.iter_mut().enumerate() {
		*byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
	}

	Some(out)
}

fn key(kind: &str, hash: &str) -> Vec<u8> {
	let mut key = PREFIX.to_vec();
	key.extend_from_slice(format!("{kind}:{hash}").as_bytes());
	key
}

/// Reads the stored list; runs once at startup.
#[implement(Service)]
pub(super) async fn load_bad_files(&self) {
	let entries: Vec<BadFile> = self
		.settings_map
		.raw_stream_prefix(PREFIX)
		.ignore_err()
		.filter_map(async |(_, value)| serde_json::from_slice(value).ok())
		.collect()
		.await;

	let mut list = self.bad_files.write().expect("locked");
	for entry in entries {
		remember(&mut list, &entry);
	}
}

fn remember(list: &mut List, entry: &BadFile) {
	let Some(hash) = unhex(&entry.hash) else {
		return;
	};
	match entry.kind.as_str() {
		| "sha256" => {
			list.sha256.insert(hash);
		},
		| "pdq" if !list.pdq.contains(&hash) => list.pdq.push(hash),
		| "pdq-frame" if !list.frames.contains(&hash) => list.frames.push(hash),
		| _ => {},
	}
}

/// How many fingerprints are on the list.
#[implement(Service)]
#[must_use]
pub fn bad_file_count(&self) -> usize {
	let list = self.bad_files.read().expect("locked");
	list.sha256
		.len()
		.saturating_add(list.pdq.len())
		.saturating_add(list.frames.len())
}

/// What on the list this file matches, if anything.
#[implement(Service)]
#[must_use]
pub fn bad_match(&self, print: &Fingerprint) -> Option<String> {
	let list = self.bad_files.read().expect("locked");
	if list.sha256.contains(&print.sha256) {
		return Some("the same file".into());
	}

	let (pdq, quality) = print.pdq?;
	if quality < PDQ_MIN_QUALITY {
		return None;
	}
	list.pdq
		.iter()
		.map(|known| pdq_distance(known, &pdq))
		.filter(|&d| d <= PDQ_MATCH)
		.min()
		.map(|d| format!("the same picture (PDQ distance {d})"))
}

/// What on the list a video matches by its frames, if anything.
#[implement(Service)]
#[must_use]
pub fn bad_frames_match(&self, frames: &[([u8; 32], u8)]) -> Option<String> {
	let good: Vec<&[u8; 32]> = frames
		.iter()
		.filter(|(_, quality)| *quality >= PDQ_MIN_QUALITY)
		.map(|(hash, _)| hash)
		.collect();
	if good.is_empty() {
		return None;
	}

	let list = self.bad_files.read().expect("locked");
	let near = |known: &[[u8; 32]], frame: &[u8; 32]| {
		known
			.iter()
			.any(|k| pdq_distance(k, frame) <= PDQ_MATCH)
	};
	if good.iter().any(|frame| near(&list.pdq, frame)) {
		return Some("a known picture, as a frame of this video".into());
	}

	let matched = good
		.iter()
		.filter(|frame| near(&list.frames, frame))
		.count();
	(matched >= FRAMES_MATCH.min(good.len()))
		.then(|| format!("the same video ({matched} of {} frames)", good.len()))
}

/// Whether these bytes are a known bad file; cheap when the list is empty.
/// A video's frames are only taken when the list has PDQ hashes to compare
/// them with.
#[implement(Service)]
pub async fn known_bad(&self, bytes: &[u8]) -> Option<String> {
	if self.bad_file_count() == 0 {
		return None;
	}

	let print = self
		.services
		.media
		.fingerprint_owned(bytes.to_vec())
		.await
		.ok()?;

	if let Some(why) = self.bad_match(&print) {
		return Some(why);
	}
	if print.pdq.is_some() || !self.has_pdq() {
		return None;
	}

	let frames = self
		.services
		.media
		.video_frame_prints(bytes)
		.await;

	self.bad_frames_match(&frames)
}

/// Whether the list has any PDQ hashes, of pictures or of video frames.
#[implement(Service)]
fn has_pdq(&self) -> bool {
	let list = self.bad_files.read().expect("locked");
	!list.pdq.is_empty() || !list.frames.is_empty()
}

/// Puts a video's frames on the list. Returns how many were new.
#[implement(Service)]
pub fn add_bad_frames(&self, frames: &[([u8; 32], u8)], note: &str, source: &str) -> usize {
	frames
		.iter()
		.filter(|(_, quality)| *quality >= PDQ_MIN_QUALITY)
		.filter(|(hash, _)| self.store_bad("pdq-frame", &hex(hash), note, source))
		.count()
}

/// Puts a file's fingerprints on the list. Returns how many were new.
#[implement(Service)]
pub fn add_bad_file(&self, print: &Fingerprint, note: &str, source: &str) -> usize {
	let mut added = 0_usize;
	let mut add = |kind: &str, hash: &[u8; 32]| {
		if self.store_bad(kind, &hex(hash), note, source) {
			added = added.saturating_add(1);
		}
	};
	add("sha256", &print.sha256);
	if let Some((pdq, quality)) = print.pdq
		&& quality >= PDQ_MIN_QUALITY
	{
		add("pdq", &pdq);
	}

	added
}

/// Stores one entry; false when it was there already or isn't valid.
#[implement(Service)]
fn store_bad(&self, kind: &str, hash: &str, note: &str, source: &str) -> bool {
	let hash = hash.trim().to_ascii_lowercase();
	if !matches!(kind, "pdq" | "pdq-frame" | "sha256") || unhex(&hash).is_none() {
		return false;
	}
	let key = key(kind, &hash);
	if self.settings_map.get_blocking(&key).is_ok() {
		return false;
	}

	let entry = BadFile {
		kind: kind.into(),
		hash,
		note: note.chars().filter(|c| !c.is_control()).take(NOTE_CHARS).collect(),
		added_ts: now_millis(),
		source: source.into(),
	};
	let Ok(value) = serde_json::to_vec(&entry) else {
		return false;
	};
	self.settings_map.insert(&key, value);
	remember(&mut self.bad_files.write().expect("locked"), &entry);

	true
}

/// Takes the lines of a shared list: `kind,hash[,note]`, where kind is
/// `pdq` or `sha256`; a line of only a hash is taken as PDQ, the usual
/// shared format, and anything after it (such as a quality) is ignored.
/// `pdq-frame` lines are a video's frames, as this server exports them.
/// Lines starting with `#` are comments. Returns (added, skipped).
#[implement(Service)]
pub fn import_bad_files(&self, text: &str, source_note: &str) -> (usize, usize) {
	let (mut added, mut skipped) = (0_usize, 0_usize);
	for line in text.lines().map(str::trim) {
		if line.is_empty() || line.starts_with('#') || line.starts_with("kind,") {
			continue;
		}
		let mut fields = line.splitn(3, [',', '\t', ' ']).map(str::trim);
		let first = fields.next().unwrap_or_default().to_ascii_lowercase();
		let (kind, hash, note) = match first.as_str() {
			| "pdq" | "pdq-frame" | "sha256" => (
				first.clone(),
				fields.next().unwrap_or_default().to_owned(),
				fields.next().unwrap_or_default().to_owned(),
			),
			| _ => ("pdq".to_owned(), first.clone(), String::new()),
		};
		let note = if note.is_empty() { source_note.to_owned() } else { note };
		if self.store_bad(&kind, &hash, &note, "import") {
			added = added.saturating_add(1);
		} else {
			skipped = skipped.saturating_add(1);
		}
	}

	(added, skipped)
}

/// Every entry, newest first.
#[implement(Service)]
pub async fn bad_files(&self) -> Vec<BadFile> {
	let mut entries: Vec<BadFile> = self
		.settings_map
		.raw_stream_prefix(PREFIX)
		.ignore_err()
		.filter_map(async |(_, value)| serde_json::from_slice(value).ok())
		.collect()
		.await;
	entries.sort_by(|a, b| b.added_ts.cmp(&a.added_ts));
	entries
}

/// The list as text, for a backup or to share with other servers' admins.
#[implement(Service)]
pub async fn export_bad_files(&self) -> String {
	let mut out = String::from(
		"# anoncat known bad files: kind,hash,note\n# Fingerprints only (SHA-256, and PDQ of \
		 pictures and video frames); no picture can be rebuilt from them.\nkind,hash,note\n",
	);
	for entry in self.bad_files().await {
		let note = entry.note.replace([',', '\n', '\r'], " ");
		out.push_str(&format!("{},{},{note}\n", entry.kind, entry.hash));
	}

	out
}

/// Takes an entry off the list.
#[implement(Service)]
pub fn remove_bad_file(&self, kind: &str, hash: &str) -> Result {
	let hash = hash.trim().to_ascii_lowercase();
	self.settings_map.remove(&key(kind, &hash));

	let Some(bytes) = unhex(&hash) else {
		return Ok(());
	};
	let mut list = self.bad_files.write().expect("locked");
	match kind {
		| "sha256" => {
			list.sha256.remove(&bytes);
		},
		| "pdq" => list.pdq.retain(|known| known != &bytes),
		| "pdq-frame" => list.frames.retain(|known| known != &bytes),
		| _ => {},
	}

	Ok(())
}

/// Progress of a scan of stored media against the list.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct BadScan {
	pub running: bool,
	pub checked: usize,
	pub total: usize,
	pub found: usize,
	pub finished_ts: Option<u64>,
}

pub(super) type ScanState = RwLock<BadScan>;

#[implement(Service)]
#[must_use]
pub fn bad_scan_status(&self) -> BadScan { self.bad_scan.read().expect("locked").clone() }

/// Checks everything stored, uploaded here or cached from elsewhere, against
/// the list, one file at a time in the background; matches are swapped and
/// reported. For after an import.
#[implement(Service)]
pub fn start_bad_scan(self: &Arc<Self>) -> Result {
	{
		let mut scan = self.bad_scan.write().expect("locked");
		if scan.running {
			return tuwunel_core::Err!(Request(Forbidden("A scan is already running.")));
		}
		*scan = BadScan { running: true, ..BadScan::default() };
	}

	let this = Arc::clone(self);
	self.services.server.runtime().spawn(async move {
		this.bad_scan().await;
		let mut scan = this.bad_scan.write().expect("locked");
		scan.running = false;
		scan.finished_ts = Some(now_millis());
	});

	Ok(())
}

#[implement(Service)]
async fn bad_scan(&self) {
	let media = &self.services.media;
	let mut mxcs: Vec<String> = media
		.cached_remote()
		.await
		.into_iter()
		.map(|item| item.mxc.to_string())
		.collect();
	for uploads in media.uploads_by_user().await.into_values() {
		mxcs.extend(uploads.into_iter().map(|u| u.mxc.to_string()));
	}
	self.bad_scan.write().expect("locked").total = mxcs.len();
	let found = |this: &Self| {
		let mut scan = this.bad_scan.write().expect("locked");
		scan.found = scan.found.saturating_add(1);
	};

	for mxc in mxcs {
		if self.services.server.is_stopping() {
			return;
		}
		if let Ok(parts) = <ruma::Mxc<'_>>::try_from(mxc.as_str())
			&& !media.is_swapped(&parts)
			&& let Ok(file) = media.get_stored(&parts).await
			&& let Some(why) = self.known_bad(&file.content).await
		{
			self.bad_file_found(&mxc, &why, "Found by a scan of stored media")
				.await;
			found(self);
		}
		let mut scan = self.bad_scan.write().expect("locked");
		scan.checked = scan.checked.saturating_add(1);
	}
}

/// A known bad file turned up: it's swapped for the stand-in for good (so
/// nobody here sees it, and other servers asking get the stand-in), reported
/// and noticed.
#[implement(Service)]
pub async fn bad_file_found(&self, mxc: &str, why: &str, origin: &str) {
	if let Err(e) = self
		.services
		.media
		.swap(mxc, &format!("known bad file: {why}"))
		.await
	{
		warn!("Couldn't swap a known bad file: {e}");
	}

	let from = <ruma::Mxc<'_>>::try_from(mxc)
		.map(|p| p.server_name.to_string())
		.unwrap_or_default();
	let mut report = self.upload_report(&from, origin);
	report.mxc = Some(mxc.to_owned());
	report.category = "known bad file".into();
	report.severity = "high".into();
	report.reason = format!("Matches the known bad files list: {why}. Swapped for the stand-in.");
	report.suggested = "review".into();
	report.hidden = true;
	self.save_report(&report);

	self.services
		.admin
		.notify_loud(&format!(
			"A known bad file from {from} was swapped for the stand-in ({why}). See Reports."
		))
		.await;
}

/// A local upload matched the list and was refused: reported, with who
/// tried, and noticed.
#[implement(Service)]
pub async fn bad_upload_refused(&self, uploader: &ruma::UserId, why: &str) {
	let mut report = self.upload_report(uploader.as_str(), "Upload refused");
	report.category = "known bad file".into();
	report.severity = "high".into();
	report.reason = format!("Tried to upload a file on the known bad files list ({why}). It was refused.");
	self.save_report(&report);

	self.services
		.admin
		.notify_loud(&format!("{uploader} tried to upload a known bad file ({why}); it was refused. See Reports."))
		.await;
}
