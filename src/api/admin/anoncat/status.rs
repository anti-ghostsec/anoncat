//! `GET /_anoncat/admin/v1/status`: server health for the panel's dashboard.

use axum::{Json, extract::State, http::HeaderMap};
use serde::Serialize;
use tuwunel_core::{
	Result,
	utils::sys::storage::{available_space, total_space},
};

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
	cleanup: Cleanup,
}

/// The cleanup limits in force, as configured.
#[derive(Serialize)]
struct Cleanup {
	interval_secs: u64,
	remote_media_max_age_secs: u64,
	local_media_max_age_secs: u64,
	local_media_max_size: u64,
	message_max_age_secs: u64,
	disk_percent: u64,
	min_free_ram: u64,
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
		cleanup: Cleanup {
			interval_secs: config.cleanup_interval.as_secs(),
			remote_media_max_age_secs: config.cleanup_remote_media_max_age.as_secs(),
			local_media_max_age_secs: config.cleanup_local_media_max_age.as_secs(),
			local_media_max_size: config.cleanup_local_media_max_size,
			message_max_age_secs: config.cleanup_message_max_age.as_secs(),
			disk_percent: config.cleanup_disk_percent,
			min_free_ram: config.cleanup_min_free_ram,
		},
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
