//! Online database backups, for the panel and the backup timer.
//!
//! RocksDB's backup engine copies the database while the server runs into
//! `database_backup_path` (inside the vault on anoncat), sharing unchanged
//! table files between backups, and keeps the newest
//! `database_backups_to_keep`. The timer (ops/backup) asks for one with
//! SIGUSR2 → `admin_signal_execute = ["server backup-database"]`; the panel
//! asks directly. Either way the run goes through here, so the panel can show
//! what happened.

use std::{
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::Instant,
};

use async_trait::async_trait;
use serde::Serialize;
use tuwunel_core::{
	Err, Result, implement, info, warn,
	utils::{
		sys::storage::{available_space, total_space},
		time::now_millis,
	},
};
use tuwunel_database::BackupInfo;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	running: AtomicBool,
	last: Mutex<Option<Outcome>>,
}

/// How the last backup since startup went.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
	pub ts: u64,
	pub millis: u64,
	/// "panel", "command" (includes the timer's signal) or another caller's.
	pub trigger: String,
	pub error: Option<String>,
}

/// One stored backup, for the panel.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Backup {
	pub id: u32,
	/// Milliseconds since the epoch.
	pub ts: u64,
	/// Bytes this backup added (shared files count in the first backup only).
	pub added: u64,
	pub files: u32,
}

/// The backup setup and what's stored.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
	/// `database_backup_path`, if set.
	pub path: Option<String>,
	pub keep: i16,
	pub running: bool,
	pub last: Option<Outcome>,
	/// Oldest first.
	pub backups: Vec<Backup>,
	/// Total size of the backup directory on disk.
	pub size_on_disk: Option<u64>,
	pub disk_total: Option<u64>,
	pub disk_available: Option<u64>,
	/// Why the list couldn't be read, if it couldn't.
	pub error: Option<String>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			running: AtomicBool::new(false),
			last: Mutex::new(None),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Makes a backup now and returns how many are stored. Refused while one is
/// running.
#[implement(Service)]
pub async fn run(&self, trigger: &str) -> Result<usize> {
	if self.running.swap(true, Ordering::AcqRel) {
		return Err!("A backup is already running.");
	}

	self.run_claimed(trigger).await
}

/// Starts a backup in the background and returns at once; refused while one
/// is running.
#[implement(Service)]
pub fn start(self: &Arc<Self>, trigger: &'static str) -> Result {
	if self.running.swap(true, Ordering::AcqRel) {
		return Err!(Request(Unknown("A backup is already running.")));
	}

	let this = self.clone();
	self.services.server.runtime().spawn(async move {
		this.run_claimed(trigger).await.ok();
	});

	Ok(())
}

/// A backup once the running flag is ours; clears it when done.
#[implement(Service)]
async fn run_claimed(&self, trigger: &str) -> Result<usize> {
	let ts = now_millis();
	let started = Instant::now();
	let db = self.services.db.clone();
	let result = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || {
			db.engine.backup()?;
			db.engine.backup_count()
		})
		.await
		.map_err(Into::into)
		.and_then(|result| result);

	self.running.store(false, Ordering::Release);
	let outcome = Outcome {
		ts,
		millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
		trigger: trigger.to_owned(),
		error: result.as_ref().err().map(ToString::to_string),
	};

	match &outcome.error {
		| None => info!(trigger, millis = outcome.millis, "Database backup made"),
		| Some(e) => {
			warn!(trigger, "Database backup failed: {e}");
			self.services
				.admin
				.notify_loud(&format!("Database backup failed: {e}"))
				.await;
		},
	}

	*self.last.lock().expect("locked") = Some(outcome);
	result
}

/// Checks a backup's files against their recorded checksums and sizes; 0 is
/// the newest.
#[implement(Service)]
pub async fn verify(&self, id: u32) -> Result<u32> {
	let db = self.services.db.clone();
	self.services
		.server
		.runtime()
		.spawn_blocking(move || db.engine.backup_verify(id))
		.await?
}

/// The setup and the stored backups.
#[implement(Service)]
pub async fn status(&self) -> Status {
	let config = &self.services.config;
	let path = config
		.database_backup_path
		.clone()
		.filter(|path| !path.as_os_str().is_empty());

	let db = self.services.db.clone();
	let scan = path.clone();
	let listed = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || -> Result<(Vec<BackupInfo>, Option<u64>)> {
			let info = db.engine.backup_info()?;
			let size = scan.as_deref().and_then(|p| dir_size(p).ok());
			Ok((info, size))
		})
		.await
		.map_err(Into::into)
		.and_then(|result| result);

	let (backups, size_on_disk, error) = match listed {
		| Ok((info, size)) => (info, size, None),
		| Err(e) => (Vec::new(), None, Some(e.to_string())),
	};

	Status {
		keep: config.database_backups_to_keep,
		running: self.running.load(Ordering::Acquire),
		last: self.last.lock().expect("locked").clone(),
		backups: backups
			.into_iter()
			.map(|b| Backup {
				id: b.id,
				ts: u64::try_from(b.timestamp)
					.unwrap_or(0)
					.saturating_mul(1000),
				added: b.size,
				files: b.files,
			})
			.collect(),
		size_on_disk,
		disk_total: path.as_deref().and_then(|p| total_space(p).ok()),
		disk_available: path
			.as_deref()
			.and_then(|p| available_space(p).ok()),
		path: path.map(|p| p.display().to_string()),
		error,
	}
}

/// Bytes in regular files under `path`, not following links.
fn dir_size(path: &std::path::Path) -> std::io::Result<u64> {
	let mut total = 0_u64;
	let mut stack = vec![path.to_path_buf()];
	while let Some(dir) = stack.pop() {
		for entry in std::fs::read_dir(&dir)? {
			let entry = entry?;
			let kind = entry.file_type()?;
			if kind.is_dir() {
				stack.push(entry.path());
			} else if !kind.is_symlink() {
				total = total.saturating_add(entry.metadata()?.len());
			}
		}
	}

	Ok(total)
}
