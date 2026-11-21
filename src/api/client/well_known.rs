use std::{
	path::Path,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
	Json,
	extract::State,
	response::{IntoResponse, Response},
};
use ruma::api::{
	OutgoingResponse,
	client::discovery::{
		discover_homeserver::{self, HomeserverInfo},
		discover_support,
	},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tuwunel_core::{Err, Result, err};

use crate::Ruma;

/// The key Whisker reads for the direct address.
const DIRECT_KEY: &str = "org.anoncat.direct";

/// How long a direct address netd hasn't refreshed is still offered.
const DIRECT_FRESH: Duration = Duration::from_secs(3 * 60);

/// # `GET /.well-known/matrix/client`
///
/// Returns the .well-known URL if it is configured, otherwise returns 404.
/// Also includes RTC transport configuration for Element Call (MSC4143),
/// and, behind a relay, the direct address (`direct_address_file`).
pub(crate) async fn well_known_client(
	State(services): State<crate::State>,
) -> Result<Response> {
	let homeserver = HomeserverInfo {
		base_url: match services.config.well_known.client.as_ref() {
			| Some(url) => url.to_string(),
			| None => return Err!(Request(NotFound("Not found."))),
		},
	};

	let rtc_foci = services.config.well_known.get_transports()?;

	let response = discover_homeserver::Response {
		rtc_foci,
		..discover_homeserver::Response::new(homeserver)
	}
	.try_into_http_response::<Vec<u8>>()
	.map_err(|e| err!(error!("Failed to build the client well-known: {e}")))?;

	let mut body: Map<String, Value> = serde_json::from_slice(response.body())?;
	if let Some(path) = services.config.direct_address_file.as_deref()
		&& let Some(base_url) = direct_base_url(path).await
	{
		body.insert(DIRECT_KEY.to_owned(), json!({ "base_url": base_url }));
	}

	Ok(Json(body).into_response())
}

/// What anoncat-netd writes, as far as this needs it.
#[derive(Deserialize)]
struct NetdState {
	#[serde(default)]
	relay: Option<String>,
	#[serde(default)]
	direct: Option<Direct>,
}

#[derive(Deserialize)]
struct Direct {
	host: String,
	port: u16,
	/// Seconds since the epoch netd last renewed the port forward.
	checked: u64,
}

/// The direct address, while a relay is in front and netd keeps it current.
async fn direct_base_url(path: &Path) -> Option<String> {
	let bytes = tokio::fs::read(path).await.ok()?;
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.ok()?
		.as_secs();

	direct_from_state(&bytes, now)
}

fn direct_from_state(bytes: &[u8], now: u64) -> Option<String> {
	let state: NetdState = serde_json::from_slice(bytes).ok()?;
	state.relay.filter(|relay| !relay.is_empty())?;

	let direct = state.direct?;
	let fresh = now.saturating_sub(direct.checked) <= DIRECT_FRESH.as_secs();
	let host_ok = !direct.host.is_empty()
		&& direct
			.host
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');

	(fresh && host_ok && direct.port != 0)
		.then(|| format!("https://{}:{}", direct.host, direct.port))
}

/// # `GET /.well-known/matrix/support`
///
/// Server support contact and support page of a homeserver's domain.
pub(crate) async fn well_known_support(
	State(services): State<crate::State>,
	_body: Ruma<discover_support::Request>,
) -> Result<discover_support::Response> {
	let config = &services.config.well_known;

	let support_page = config
		.support_page
		.as_ref()
		.map(ToString::to_string);

	let contacts = config.get_contacts();

	let policies = config.get_policies();

	if support_page.is_none() && contacts.is_empty() && policies.is_empty() {
		return Err!(Request(NotFound("Not found.")));
	}

	Ok(discover_support::Response { contacts, support_page, policies })
}

#[cfg(test)]
mod tests {
	use super::direct_from_state;

	const NOW: u64 = 1_000_000;

	#[test]
	fn direct_is_offered_only_behind_a_relay_and_while_fresh() {
		let state = |relay: &str, checked: u64| {
			format!(
				r#"{{"relay":"{relay}","direct":{{"host":"direct.example.com","port":53383,"checked":{checked}}}}}"#
			)
		};

		assert_eq!(
			direct_from_state(state("192.0.2.1", NOW - 30).as_bytes(), NOW).as_deref(),
			Some("https://direct.example.com:53383")
		);
		assert_eq!(direct_from_state(state("", NOW).as_bytes(), NOW), None);
		assert_eq!(direct_from_state(state("192.0.2.1", NOW - 600).as_bytes(), NOW), None);
		assert_eq!(direct_from_state(br#"{"ip":"192.0.2.1","port":443}"#, NOW), None);
		assert_eq!(direct_from_state(b"not json", NOW), None);
	}

	#[test]
	fn direct_host_cant_inject_into_the_url() {
		let state = format!(
			r#"{{"relay":"192.0.2.1","direct":{{"host":"evil.example/x?","port":1,"checked":{NOW}}}}}"#
		);
		assert_eq!(direct_from_state(state.as_bytes(), NOW), None);
	}
}
