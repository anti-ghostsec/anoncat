use std::time::Instant;

use axum::{Json, extract::State, response::IntoResponse};
use ruma::api::{client::tuwunel::get_remote_version, federation::discovery::get_server_version};
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_tuwunel/server_version`
///
/// Tuwunel-specific API to get the server version, results akin to
/// `/_matrix/federation/v1/version`
///
/// Kept for `--health-check`, which only needs an answer; the software and
/// version aren't given out.
pub(crate) async fn tuwunel_server_version() -> Result<impl IntoResponse> {
	Ok(Json(serde_json::json!({})))
}

/// # `GET /_tuwunel/remote_version/{server_name}`
///
/// Tuwunel-specific API to probe a remote server's
/// `/_matrix/federation/v1/version` endpoint, returning that response body
/// along with the round-trip time of the probe.
pub(crate) async fn tuwunel_remote_version(
	State(services): State<crate::State>,
	body: Ruma<get_remote_version::unstable::Request>,
) -> Result<get_remote_version::unstable::Response> {
	let timer = Instant::now();

	// This server's own version stays unsaid here too, as it does over
	// federation.
	let server = if services.globals.server_is_ours(&body.server_name) {
		None
	} else {
		services
			.federation
			.execute(&body.server_name, get_server_version::v1::Request {})
			.await?
			.server
	};

	let elapsed = timer.elapsed();

	Ok(get_remote_version::unstable::Response {
		data: serde_json::value::to_raw_value(&server)?,
		rtt_ms: elapsed,
	})
}
