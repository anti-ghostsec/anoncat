//! `GET /_anoncat/admin/v1/logs`: the server's recent log lines, from memory,
//! with IP addresses blanked.

use axum::{Json, extract::State, http::HeaderMap, http::Uri};
use serde::Serialize;
use tuwunel_core::{Result, log::Level};
use tuwunel_service::logs::Line;

/// Most lines one request returns.
const LIMIT: usize = 2000;

#[derive(Serialize)]
pub(crate) struct Response {
	lines: Vec<Line>,
}

/// `?level=error|warn|info` (default info), `?limit=N`.
pub(crate) async fn get_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;

	let mut level = Level::INFO;
	let mut limit = 500;
	for (key, value) in uri.query().unwrap_or_default().split('&').filter_map(|p| p.split_once('=')) {
		match key {
			| "level" => {
				level = match value {
					| "error" => Level::ERROR,
					| "warn" => Level::WARN,
					| _ => Level::INFO,
				};
			},
			| "limit" => limit = value.parse().unwrap_or(limit).min(LIMIT),
			| _ => {},
		}
	}

	Ok(Json(Response { lines: services.logs.lines(level, limit) }))
}
