//! `GET /_anoncat/admin/v1/status`: server health for the panel's dashboard.

use axum::{Json, extract::State, http::HeaderMap};
use serde::Serialize;
use tuwunel_core::{
	Result,
	utils::sys::storage::{available_space, total_space},
};
use tuwunel_service::cleanup::Limits;

#[derive(Serialize)]
pub(crate) struct Response {
	server_name: String,
	version: String,
	uptime_secs: u64,
	disk_total: Option<u64>,
	disk_available: Option<u64>,
	ram_total: Option<u64>,
	ram_available: Option<u64>,
	rss: Option<u64>,
	/// The service's own memory, as systemd counts it (caches included), and
	/// its limits: past `memory_high` it's slowed while the kernel takes
	/// memory back; `memory_max` is the hard stop.
	memory_current: Option<u64>,
	memory_high: Option<u64>,
	memory_max: Option<u64>,
	/// Times it was held back at `memory_high` since it started.
	memory_high_events: Option<u64>,
	/// Whether the public address answered when last checked.
	reachable: tuwunel_service::moderation::Reachability,
	/// Reports waiting for an admin, for the menu's badge.
	open_reports: usize,
	/// The cleanup limits in force (panel overrides, else the config).
	cleanup: Limits,
}

pub(crate) async fn anoncat_status_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;
	let config = &services.config;
	let path = &config.database_path;
	let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
	let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();

	Ok(Json(Response {
		server_name: services.globals.server_name().to_string(),
		version: tuwunel_core::version::version().to_owned(),
		uptime_secs: services.server.started.elapsed().map_or(0, |d| d.as_secs()),
		disk_total: total_space(path).ok(),
		disk_available: available_space(path).ok(),
		ram_total: kib_field(&meminfo, "MemTotal:"),
		ram_available: kib_field(&meminfo, "MemAvailable:"),
		rss: kib_field(&status, "VmRSS:"),
		memory_current: cgroup_value("memory.current"),
		memory_high: cgroup_value("memory.high"),
		memory_max: cgroup_value("memory.max"),
		memory_high_events: cgroup_file("memory.events").and_then(|events| {
			events
				.lines()
				.find_map(|line| line.strip_prefix("high "))
				.and_then(|n| n.trim().parse().ok())
		}),
		reachable: services.moderation.reachability(),
		open_reports: services.moderation.reports(false, usize::MAX).await.len(),
		cleanup: services.cleanup.limits(),
	}))
}

/// Reads a `Name:   1234 kB` line from a /proc file, in bytes.
fn kib_field(text: &str, name: &str) -> Option<u64> {
	text.lines()
		.find_map(|line| line.strip_prefix(name))?
		.trim()
		.trim_end_matches("kB")
		.trim()
		.parse::<u64>()
		.ok()
		.map(|kib| kib.saturating_mul(1024))
}

/// A file of this process's own control group (cgroup v2).
fn cgroup_file(name: &str) -> Option<String> {
	let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
	let path = own
		.lines()
		.find_map(|line| line.strip_prefix("0::"))?
		.trim();

	std::fs::read_to_string(format!("/sys/fs/cgroup{path}/{name}")).ok()
}

/// A number from one; "max" (no limit) reads as none.
fn cgroup_value(name: &str) -> Option<u64> { cgroup_file(name)?.trim().parse().ok() }
