//! Scheduled cleanup.
//!
//! Expires media and message history by the `cleanup_*` limits and guards the
//! disk and memory, so a server with fixed resources doesn't fill up over
//! time. Each run that deletes anything posts a notice.
//!
//! The panel can override any limit at runtime; overrides are stored in the
//! database and win over the config file until reset. The last runs are kept
//! for the panel too.

use std::{
	fmt::Write,
	path::Path,
	sync::{
		Arc, Mutex, RwLock,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use futures::{StreamExt, pin_mut};
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, debug_info, implement, info,
	utils::{
		sys::storage::{available_space, total_space},
		time::now_millis,
	},
	warn,
};

/// How far ahead of this server's clock an event may be dated and still
/// count as when it was sent.
const FUTURE_SLACK_MS: u64 = 24 * 60 * 60 * 1000;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const MIB: u64 = 1 << 20;

/// Remote media ages the disk guard works through, oldest first.
const DISK_GUARD_STEPS: [Duration; 4] =
	[DAY.saturating_mul(7), DAY.saturating_mul(3), DAY, Duration::ZERO];

/// Wait after startup before the first run, so it doesn't compete with the
/// prewarm for the disk.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(5 * 60);

const RAM_WARNING_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Keys in `anoncat_settings`.
const OVERRIDES_KEY: &str = "cleanup_overrides";
const HISTORY_KEY: &str = "cleanup_history";

/// Runs kept in the history.
const HISTORY_KEPT: usize = 50;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	settings: Arc<tuwunel_database::Map>,
	overrides: RwLock<Overrides>,
	running: AtomicBool,
	history: tokio::sync::Mutex<()>,
	last_ram_warning: Mutex<Option<Instant>>,
}

/// Limits set from the panel, each replacing its `cleanup_*` config option.
/// Durations in seconds, sizes in bytes; 0 turns a rule off.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overrides {
	pub interval_secs: Option<u64>,
	pub remote_media_max_age_secs: Option<u64>,
	pub local_media_max_age_secs: Option<u64>,
	pub local_media_max_size: Option<u64>,
	pub message_max_age_secs: Option<u64>,
	pub disk_percent: Option<u64>,
	pub min_free_ram: Option<u64>,
}

/// The limits in force: an override where there is one, else the config.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Limits {
	pub interval_secs: u64,
	pub remote_media_max_age_secs: u64,
	pub local_media_max_age_secs: u64,
	pub local_media_max_size: u64,
	pub message_max_age_secs: u64,
	pub disk_percent: u64,
	pub min_free_ram: u64,
}

impl Limits {
	fn interval(&self) -> Duration { Duration::from_secs(self.interval_secs) }

	fn remote_media_max_age(&self) -> Duration { Duration::from_secs(self.remote_media_max_age_secs) }

	fn local_media_max_age(&self) -> Duration { Duration::from_secs(self.local_media_max_age_secs) }

	fn message_max_age(&self) -> Duration { Duration::from_secs(self.message_max_age_secs) }
}

/// One finished (or failed) run, as the panel lists it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
	/// When it started, in milliseconds since the epoch.
	pub ts: u64,
	pub millis: u64,
	/// "scheduled", "panel" or "command".
	pub trigger: String,
	pub error: Option<String>,
	pub report: Report,
}

/// What one cleanup run deleted.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Report {
	pub remote_media: usize,
	pub local_media_expired: usize,
	pub local_media_over_limit: usize,
	pub local_media_freed_mib: u64,
	pub events_purged: usize,
	pub rooms_purged: usize,
	pub disk_guard_media: usize,
	pub disk_percent: Option<u64>,
}

impl Report {
	fn deleted_anything(&self) -> bool {
		self.remote_media
			.saturating_add(self.local_media_expired)
			.saturating_add(self.local_media_over_limit)
			.saturating_add(self.events_purged)
			.saturating_add(self.disk_guard_media)
			> 0
	}
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			settings: args.db["anoncat_settings"].clone(),
			overrides: RwLock::default(),
			running: AtomicBool::new(false),
			history: tokio::sync::Mutex::new(()),
			last_ram_warning: Mutex::new(None),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		if let Ok(stored) = self.settings.get(OVERRIDES_KEY).await {
			match serde_json::from_slice::<Overrides>(&stored) {
				| Ok(stored) => *self.overrides.write().expect("locked") = stored,
				| Err(e) => warn!("Cleanup overrides unreadable, using the config file: {e}"),
			}
		}

		let mut wait = FIRST_RUN_DELAY;
		loop {
			tokio::select! {
				() = tokio::time::sleep(wait) => {},
				() = self.services.server.until_shutdown() => return Ok(()),
			}

			if let Err(e) = self.run("scheduled").await {
				warn!("Cleanup run failed: {e}");
			}

			wait = self.limits().interval().max(Duration::from_secs(60));
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// The overrides set from the panel.
#[implement(Service)]
#[must_use]
pub fn overrides(&self) -> Overrides { *self.overrides.read().expect("locked") }

/// Replaces the overrides (all of them; `None` falls back to the config).
#[implement(Service)]
pub fn set_overrides(&self, overrides: Overrides) -> Result<Limits> {
	if overrides.disk_percent.is_some_and(|p| p > 100) {
		return Err!(Request(InvalidParam("The disk guard is a percentage, 0 to 100.")));
	}

	if overrides.interval_secs.is_some_and(|s| s < 60) {
		return Err!(Request(InvalidParam("Cleanup can't run more often than once a minute.")));
	}

	self.settings
		.insert(OVERRIDES_KEY, serde_json::to_vec(&overrides)?);
	*self.overrides.write().expect("locked") = overrides;
	info!(?overrides, "Cleanup limits changed from the panel");

	Ok(self.limits())
}

/// The limits from the config file alone.
#[implement(Service)]
#[must_use]
pub fn configured(&self) -> Limits {
	let config = &self.services.config;
	Limits {
		interval_secs: config.cleanup_interval.as_secs(),
		remote_media_max_age_secs: config.cleanup_remote_media_max_age.as_secs(),
		local_media_max_age_secs: config.cleanup_local_media_max_age.as_secs(),
		local_media_max_size: config.cleanup_local_media_max_size,
		message_max_age_secs: config.cleanup_message_max_age.as_secs(),
		disk_percent: config.cleanup_disk_percent,
		min_free_ram: config.cleanup_min_free_ram,
	}
}

/// The limits in force.
#[implement(Service)]
#[must_use]
pub fn limits(&self) -> Limits {
	let c = self.configured();
	let o = self.overrides();
	Limits {
		interval_secs: o.interval_secs.unwrap_or(c.interval_secs),
		remote_media_max_age_secs: o
			.remote_media_max_age_secs
			.unwrap_or(c.remote_media_max_age_secs),
		local_media_max_age_secs: o
			.local_media_max_age_secs
			.unwrap_or(c.local_media_max_age_secs),
		local_media_max_size: o.local_media_max_size.unwrap_or(c.local_media_max_size),
		message_max_age_secs: o.message_max_age_secs.unwrap_or(c.message_max_age_secs),
		disk_percent: o.disk_percent.unwrap_or(c.disk_percent),
		min_free_ram: o.min_free_ram.unwrap_or(c.min_free_ram),
	}
}

/// Whether a run is going on now.
#[implement(Service)]
#[must_use]
pub fn is_running(&self) -> bool { self.running.load(Ordering::Acquire) }

/// The last runs, newest first.
#[implement(Service)]
pub async fn history(&self) -> Vec<Run> {
	self.settings
		.get(HISTORY_KEY)
		.await
		.ok()
		.and_then(|stored| serde_json::from_slice(&stored).ok())
		.unwrap_or_default()
}

#[implement(Service)]
async fn record(&self, run: Run) {
	let _lock = self.history.lock().await;
	let mut history = self.history().await;
	history.insert(0, run);
	history.truncate(HISTORY_KEPT);
	if let Ok(value) = serde_json::to_vec(&history) {
		self.settings.insert(HISTORY_KEY, value);
	}
}

/// Runs every enabled cleanup once, records the run and reports what it
/// deleted. Refused while another run is going on.
#[implement(Service)]
pub async fn run(&self, trigger: &str) -> Result<Report> {
	if self.running.swap(true, Ordering::AcqRel) {
		return Err!("A cleanup is already running.");
	}

	self.run_claimed(trigger).await
}

/// Starts a run in the background and returns at once; refused while one is
/// running.
#[implement(Service)]
pub fn start(self: &Arc<Self>, trigger: &'static str) -> Result {
	if self.running.swap(true, Ordering::AcqRel) {
		return Err!(Request(Unknown("A cleanup is already running.")));
	}

	let this = self.clone();
	self.services.server.runtime().spawn(async move {
		if let Err(e) = this.run_claimed(trigger).await {
			warn!(trigger, "Cleanup run failed: {e}");
		}
	});

	Ok(())
}

/// A run once the running flag is ours; clears it when done.
#[implement(Service)]
async fn run_claimed(&self, trigger: &str) -> Result<Report> {
	let ts = now_millis();
	let started = Instant::now();
	let result = self.run_once().await;
	self.running.store(false, Ordering::Release);

	self.record(Run {
		ts,
		millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
		trigger: trigger.to_owned(),
		error: result.as_ref().err().map(ToString::to_string),
		report: result.as_ref().copied().unwrap_or_default(),
	})
	.await;

	result
}

#[implement(Service)]
async fn run_once(&self) -> Result<Report> {
	let limits = self.limits();
	let mut report = Report::default();

	self.guard_ram(&limits).await;

	if !limits.remote_media_max_age().is_zero() {
		report.remote_media = self
			.delete_remote_media_older_than(limits.remote_media_max_age())
			.await?;
	}

	if !limits.local_media_max_age().is_zero() {
		let max_age = u64::try_from(limits.local_media_max_age().as_millis())?;
		let cutoff = now_millis().saturating_sub(max_age);

		report.local_media_expired = self
			.services
			.media
			.delete_by_date_size(cutoff, 0, true)
			.await?
			.len();
	}

	if limits.local_media_max_size > 0 {
		let (deleted, freed) = self
			.services
			.media
			.delete_oldest_local_over(limits.local_media_max_size)
			.await?;

		report.local_media_over_limit = deleted;
		report.local_media_freed_mib = freed / MIB;
	}

	if !limits.message_max_age().is_zero() {
		(report.events_purged, report.rooms_purged) = self
			.purge_history_older_than(limits.message_max_age())
			.await;
	}

	(report.disk_guard_media, report.disk_percent) = self.guard_disk(&limits).await?;

	if report.deleted_anything() {
		let summary = summarize(&report);
		info!("{summary}");
		self.services.admin.notify(&summary).await;
	} else {
		debug_info!(?report, "Cleanup found nothing to delete");
	}

	Ok(report)
}

#[implement(Service)]
async fn delete_remote_media_older_than(&self, max_age: Duration) -> Result<usize> {
	let cutoff = SystemTime::now()
		.checked_sub(max_age)
		.unwrap_or(SystemTime::UNIX_EPOCH);

	self.services
		.media
		.delete_range(cutoff, true, false, false)
		.await
}

/// Purges events older than `max_age` from every room, keeping state.
/// Returns the events purged and the rooms they came from.
#[implement(Service)]
async fn purge_history_older_than(&self, max_age: Duration) -> (usize, usize) {
	let cutoff = MilliSecondsSinceUnixEpoch::from_system_time(
		SystemTime::now()
			.checked_sub(max_age)
			.unwrap_or(SystemTime::UNIX_EPOCH),
	)
	.unwrap_or_else(MilliSecondsSinceUnixEpoch::now);

	let rooms: Vec<OwnedRoomId> = self
		.services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let admin_room = self.services.admin.get_admin_room().await.ok();
	let (mut events, mut purged_rooms) = (0_usize, 0_usize);
	for room_id in rooms {
		if !self.services.server.is_running() {
			break;
		}

		// The admin room's history is the record of what admins did.
		if admin_room.as_deref() == Some(&*room_id) {
			continue;
		}

		// Walk the timeline in the order events arrived and stop at the first
		// one newer than the cutoff: everything before it goes. Timestamps are
		// chosen by the sending server: one dated in the future is passed
		// over, so a single bad clock can't stop expiry in a room for good.
		let timeline = self.services.timeline.pdus(None, &room_id, None);
		pin_mut!(timeline);
		let mut boundary = None;
		let future = now_millis().saturating_add(FUTURE_SLACK_MS);
		while let Some(Ok((count, pdu))) = timeline.next().await {
			boundary = Some(count);
			let ts = u64::from(pdu.origin_server_ts);
			if ts > future {
				continue;
			}
			if pdu.origin_server_ts > cutoff.get() {
				break;
			}
		}

		let Some(boundary) = boundary else {
			continue;
		};

		match self
			.services
			.timeline
			.purge_history(&room_id, boundary, true)
			.await
		{
			| Ok(0) => {},
			| Ok(purged) => {
				events = events.saturating_add(purged);
				purged_rooms = purged_rooms.saturating_add(1);
			},
			| Err(e) => warn!(%room_id, "Cleanup could not purge history: {e}"),
		}
	}

	(events, purged_rooms)
}

/// Deletes remote media, older first, while the disk is over the limit.
/// Returns the media deleted and the disk use it ended at.
#[implement(Service)]
async fn guard_disk(&self, limits: &Limits) -> Result<(usize, Option<u64>)> {
	let limit = limits.disk_percent;
	let path = &self.services.config.database_path;
	let mut used = disk_used_percent(path);
	if limit == 0 || used.is_none_or(|used| used < limit) {
		return Ok((0, used));
	}

	let mut deleted = 0_usize;
	for max_age in DISK_GUARD_STEPS {
		deleted = deleted.saturating_add(self.delete_remote_media_older_than(max_age).await?);
		used = disk_used_percent(path);
		if used.is_none_or(|used| used < limit) {
			return Ok((deleted, used));
		}
	}

	let used = used.unwrap_or(100);
	self.services
		.admin
		.notify_loud(&format!(
			"The disk holding the database is {used}% full (limit {limit}%) even after \
			 deleting all remote media. Free space or lower the cleanup limits before it \
			 fills."
		))
		.await;

	Ok((deleted, Some(used)))
}

/// Clears caches and warns when the system is low on memory.
#[implement(Service)]
async fn guard_ram(&self, limits: &Limits) {
	let min = limits.min_free_ram / MIB;
	let Some(available) = available_ram_mib() else {
		return;
	};

	if min == 0 || available >= min {
		return;
	}

	self.services.clear_cache().await;

	let warn_now = {
		let mut last = self.last_ram_warning.lock().expect("locked");
		let due = last.is_none_or(|last| last.elapsed() >= RAM_WARNING_INTERVAL);
		if due {
			*last = Some(Instant::now());
		}

		due
	};

	if warn_now {
		self.services
			.admin
			.notify_loud(&format!(
				"Low memory: {available} MiB available (limit {min} MiB). Caches were cleared; \
				 lower db_cache_capacity_mb or cache_capacity_modifier if this repeats."
			))
			.await;
	}
}

fn summarize(report: &Report) -> String {
	let mut out = String::from("Cleanup:");
	let mut add = |count: usize, what: &str| {
		if count > 0 {
			write!(out, " {count} {what};").expect("writes to a string");
		}
	};

	add(report.remote_media, "remote media files expired");
	add(report.local_media_expired, "local uploads expired");
	add(report.local_media_over_limit, "oldest local uploads deleted to stay under the size limit");
	add(report.disk_guard_media, "remote media files deleted because the disk was full");

	if report.events_purged > 0 {
		write!(out, " {} old events purged from {} rooms;", report.events_purged, report.rooms_purged)
			.expect("writes to a string");
	}

	if let Some(used) = report.disk_percent {
		write!(out, " disk {used}% used.").expect("writes to a string");
	}

	out
}

fn disk_used_percent(path: &Path) -> Option<u64> {
	let total = total_space(path).ok()?;
	let available = available_space(path).ok()?;
	let used = total.saturating_sub(available);

	used.saturating_mul(100).checked_div(total)
}

fn available_ram_mib() -> Option<u64> {
	std::fs::read_to_string("/proc/meminfo")
		.ok()?
		.lines()
		.find_map(|line| line.strip_prefix("MemAvailable:"))?
		.trim()
		.trim_end_matches("kB")
		.trim()
		.parse::<u64>()
		.ok()
		.map(|kib| kib / 1024)
}
