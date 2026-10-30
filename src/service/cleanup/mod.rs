//! Scheduled cleanup.
//!
//! Expires media and message history by the `cleanup_*` limits and guards the
//! disk and memory, so a server with fixed resources doesn't fill up over
//! time. Each run that deletes anything reports to the admin room.

use std::{
	fmt::Write,
	path::Path,
	sync::{Arc, Mutex},
	time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use futures::{StreamExt, pin_mut};
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, api::Direction};
use tuwunel_core::{
	Result, debug_info, implement, info,
	utils::{
		sys::storage::{available_space, total_space},
		time::now_millis,
	},
	warn,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const MIB: u64 = 1 << 20;

/// Remote media ages the disk guard works through, oldest first.
const DISK_GUARD_STEPS: [Duration; 4] =
	[DAY.saturating_mul(7), DAY.saturating_mul(3), DAY, Duration::ZERO];

/// Wait after startup before the first run, so it doesn't compete with the
/// prewarm for the disk.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(5 * 60);

const RAM_WARNING_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	last_ram_warning: Mutex<Option<Instant>>,
}

/// What one cleanup run deleted.
#[derive(Clone, Copy, Debug, Default)]
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
			last_ram_warning: Mutex::new(None),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let mut wait = FIRST_RUN_DELAY;
		loop {
			tokio::select! {
				() = tokio::time::sleep(wait) => {},
				() = self.services.server.until_shutdown() => return Ok(()),
			}

			if let Err(e) = self.run().await {
				warn!("Cleanup run failed: {e}");
			}

			wait = self
				.services
				.config
				.cleanup_interval
				.max(Duration::from_secs(60));
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Runs every enabled cleanup once and reports what it deleted.
#[implement(Service)]
pub async fn run(&self) -> Result<Report> {
	let config = &self.services.config;
	let mut report = Report::default();

	self.guard_ram().await;

	if !config.cleanup_remote_media_max_age.is_zero() {
		report.remote_media = self
			.delete_remote_media_older_than(config.cleanup_remote_media_max_age)
			.await?;
	}

	if !config.cleanup_local_media_max_age.is_zero() {
		let max_age = u64::try_from(config.cleanup_local_media_max_age.as_millis())?;
		let cutoff = now_millis().saturating_sub(max_age);

		report.local_media_expired = self
			.services
			.media
			.delete_by_date_size(cutoff, 0, true)
			.await?
			.len();
	}

	if config.cleanup_local_media_max_size > 0 {
		let (deleted, freed) = self
			.services
			.media
			.delete_oldest_local_over(config.cleanup_local_media_max_size)
			.await?;

		report.local_media_over_limit = deleted;
		report.local_media_freed_mib = freed / MIB;
	}

	if !config.cleanup_message_max_age.is_zero() {
		(report.events_purged, report.rooms_purged) = self
			.purge_history_older_than(config.cleanup_message_max_age)
			.await;
	}

	(report.disk_guard_media, report.disk_percent) = self.guard_disk().await?;

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

	let (mut events, mut purged_rooms) = (0_usize, 0_usize);
	for room_id in rooms {
		if !self.services.server.is_running() {
			break;
		}

		// The newest event at or before the cutoff; everything before it goes.
		let boundary = self
			.services
			.timeline
			.pdus_near_ts(None, &room_id, cutoff, Direction::Backward);

		pin_mut!(boundary);
		let Some(Ok((boundary, _))) = boundary.next().await else {
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
async fn guard_disk(&self) -> Result<(usize, Option<u64>)> {
	let limit = self.services.config.cleanup_disk_percent;
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
async fn guard_ram(&self) {
	let min = self.services.config.cleanup_min_free_ram / MIB;
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
