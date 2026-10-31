//! The panel's Cleanup page: the limits in force, runtime overrides of the
//! `cleanup_*` options, a run on demand and the last runs.

use axum::{Json, extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuwunel_core::{
	Result,
	utils::{bytes, time::parse_duration},
	warn,
};
use tuwunel_service::cleanup::{Limits, Overrides, Run};

#[derive(Serialize)]
pub(crate) struct Response {
	/// In force now.
	limits: Limits,
	/// From the config file alone.
	configured: Limits,
	overrides: Overrides,
	running: bool,
	history: Vec<Run>,
}

pub(crate) async fn get_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(response(&services).await))
}

async fn response(services: &crate::State) -> Response {
	let cleanup = &services.cleanup;
	Response {
		limits: cleanup.limits(),
		configured: cleanup.configured(),
		overrides: cleanup.overrides(),
		running: cleanup.is_running(),
		history: cleanup.history().await,
	}
}

/// Each field is null (use the config file), a number (seconds or bytes) or
/// text as in the config file ("30d", "1 GiB", "0" for off).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutRequest {
	interval: Value,
	remote_media_max_age: Value,
	local_media_max_age: Value,
	local_media_max_size: Value,
	message_max_age: Value,
	#[serde(default)]
	media_message_max_age: Value,
	disk_percent: Value,
	min_free_ram: Value,
}

pub(crate) async fn put_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<PutRequest>,
) -> Result<Json<Response>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	let overrides = Overrides {
		interval_secs: duration("Runs every", &body.interval)?,
		remote_media_max_age_secs: duration("Remote media", &body.remote_media_max_age)?,
		local_media_max_age_secs: duration("Local uploads", &body.local_media_max_age)?,
		local_media_max_size: size("Local uploads size cap", &body.local_media_max_size)?,
		message_max_age_secs: duration("Message history", &body.message_max_age)?,
		media_message_max_age_secs: duration(
			"Messages with media",
			&body.media_message_max_age,
		)?,
		disk_percent: number("Disk guard", &body.disk_percent)?,
		min_free_ram: size("Low-memory guard", &body.min_free_ram)?,
	};

	services.cleanup.set_overrides(overrides)?;
	warn!(%admin, "Cleanup limits changed from the panel");

	Ok(Json(response(&services).await))
}

/// Starts a run in the background; the page polls for it to finish.
pub(crate) async fn run_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;
	services.cleanup.start("panel")?;
	Ok(Json(response(&services).await))
}

fn duration(what: &str, value: &Value) -> Result<Option<u64>> {
	match value {
		| Value::Null => Ok(None),
		| Value::Number(n) => n
			.as_u64()
			.map(Some)
			.ok_or_else(|| bad(what)),
		| Value::String(s) if s.trim().is_empty() => Ok(None),
		| Value::String(s) if s.trim() == "0" => Ok(Some(0)),
		| Value::String(s) => parse_duration(s.trim())
			.map(|d| Some(d.as_secs()))
			.map_err(|_| bad(what)),
		| _ => Err(bad(what)),
	}
}

fn size(what: &str, value: &Value) -> Result<Option<u64>> {
	match value {
		| Value::Null => Ok(None),
		| Value::Number(n) => n
			.as_u64()
			.map(Some)
			.ok_or_else(|| bad(what)),
		| Value::String(s) if s.trim().is_empty() => Ok(None),
		| Value::String(s) if s.trim() == "0" => Ok(Some(0)),
		| Value::String(s) => bytes::from_str(s.trim())
			.ok()
			.and_then(|b| u64::try_from(b).ok())
			.map(Some)
			.ok_or_else(|| bad(what)),
		| _ => Err(bad(what)),
	}
}

fn number(what: &str, value: &Value) -> Result<Option<u64>> {
	match value {
		| Value::Null => Ok(None),
		| Value::Number(n) => n
			.as_u64()
			.map(Some)
			.ok_or_else(|| bad(what)),
		| Value::String(s) if s.trim().is_empty() => Ok(None),
		| Value::String(s) => s
			.trim()
			.trim_end_matches('%')
			.parse()
			.map(Some)
			.map_err(|_| bad(what)),
		| _ => Err(bad(what)),
	}
}

fn bad(what: &str) -> tuwunel_core::Error {
	tuwunel_core::err!(Request(InvalidParam("{what}: not a value I understand.")))
}
