//! Moderation shared by the twins: the twin side runs the same AI checks as
//! the source, with the source's settings and AI key, and both refuse the
//! same known bad files.
//!
//! The source sends its settings, key and bad-files list whenever any of
//! them changes, and whenever the twin side's list has a different count
//! (it swapped something itself); the twin side answers with its own list,
//! which the source adds to its own. Removing an entry takes removing it on
//! both, as the panel does.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuwunel_core::{Err, Result, debug_warn, implement, info, utils::hash::sha256};

use super::Service;

/// What the source sends.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
	/// The moderation settings, as the panel's Settings page has them.
	pub settings: Value,
	/// The chosen AI service's key; none when the source has none.
	#[serde(default)]
	pub key: Option<String>,
	/// The known bad files, as exported.
	#[serde(default)]
	pub bad_files: String,
}

/// What the twin side answers.
#[derive(Deserialize, Serialize)]
pub struct MirrorAnswer {
	/// Its known bad files after taking the source's, as exported.
	pub bad_files: String,
	pub bad_file_count: usize,
}

/// The twin side's count, to tell whether it has files the source lacks.
#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct MirrorState {
	pub bad_file_count: usize,
}

/// Twin side: takes the source's settings, key and bad files.
#[implement(Service)]
pub async fn apply_moderation(&self, mirror: Mirror) -> Result<MirrorAnswer> {
	let services = &self.services;
	let moderation = &services.moderation;

	let extra: Vec<String> = mirror
		.settings
		.get("extra_rooms")
		.and_then(|rooms| serde_json::from_value(rooms.clone()).ok())
		.unwrap_or_default();

	let Ok(settings) = serde_json::from_value(mirror.settings) else {
		return Err!(Request(BadJson("Settings didn't parse.")));
	};

	moderation.set_settings(settings, &extra).await?;

	if let Some(key) = mirror.key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
		match moderation.key_path() {
			| Some(path) if moderation.key().as_deref() != Some(key) => {
				write_key(&path, key).await?;
				info!("AI key taken from the source server");
			},
			| Some(_) => {},
			| None => debug_warn!("No ai_key_file set here; the AI key isn't kept"),
		}
	}

	let (added, _) = moderation.import_bad_files(&mirror.bad_files, "from the other anoncat server");
	if added > 0 {
		info!(added, "Known bad files taken from the source server");
	}

	Ok(MirrorAnswer {
		bad_files: moderation.export_bad_files().await,
		bad_file_count: moderation.bad_file_count(),
	})
}

/// Twin side: its bad-files count.
#[implement(Service)]
#[must_use]
pub fn moderation_state(&self) -> MirrorState {
	MirrorState {
		bad_file_count: self.services.moderation.bad_file_count(),
	}
}

/// Source side, each run: sends the moderation state when it changed, or
/// when the twin side's bad-files list differs in size, and adds the twin
/// side's bad files to this one's.
#[implement(Service)]
pub(super) async fn sync_moderation(&self, api: &super::source::Api<'_>) -> Result<(), ()> {
	use super::source::{CallError, call};

	let moderation = &self.services.moderation;
	let settings = serde_json::to_value(moderation.settings()).unwrap_or_default();
	let key = moderation.key();
	let bad_files = moderation.export_bad_files().await;
	let digest = sha256::hash(
		serde_json::to_vec(&(&settings, &key, &bad_files)).unwrap_or_default(),
	);

	let state_path = ["_anoncat", "twin", "v1", "moderation"];
	let theirs: MirrorState = match call(api, Method::GET, &state_path, &()).await {
		| Ok(state) => state,
		| Err(CallError::Unreachable) => return Err(()),
		| Err(CallError::Refused(e) | CallError::Failed(e)) => {
			debug_warn!("Twin moderation state not read: {e}");
			return Ok(());
		},
	};

	let unchanged = *self.moderation_pushed.lock().expect("locked") == Some(digest);
	if unchanged && theirs.bad_file_count == moderation.bad_file_count() {
		return Ok(());
	}

	let mirror = Mirror { settings, key, bad_files };
	match call::<_, MirrorAnswer>(api, Method::PUT, &state_path, &mirror).await {
		| Ok(answer) => {
			let (added, _) = moderation.import_bad_files(&answer.bad_files, "from the other anoncat server");
			if added > 0 {
				info!(added, "Known bad files taken from the twin server");
			}

			// What was sent is in step; files just taken change the digest,
			// so they go back next run and the counts meet.
			*self.moderation_pushed.lock().expect("locked") = Some(digest);
			Ok(())
		},
		| Err(CallError::Unreachable) => Err(()),
		| Err(CallError::Refused(e) | CallError::Failed(e)) => {
			tuwunel_core::warn!("Moderation settings not sent to the twin server: {e}");
			Ok(())
		},
	}
}

/// Writes the key readable by this server only.
async fn write_key(path: &std::path::Path, key: &str) -> Result {
	use std::os::unix::fs::OpenOptionsExt;

	let path = path.to_owned();
	let key = format!("{key}\n");
	tokio::task::spawn_blocking(move || -> Result {
		use std::io::Write;

		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)?;
		}

		let tmp = path.with_extension("new");
		let mut file = std::fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.mode(0o600)
			.open(&tmp)?;
		file.write_all(key.as_bytes())?;
		file.sync_all()?;
		std::fs::rename(&tmp, &path)?;
		Ok(())
	})
	.await?
}
