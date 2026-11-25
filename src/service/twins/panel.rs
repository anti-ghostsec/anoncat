//! The source side's admin panel runs the twin side too: requests the panel
//! sends for the twin side are passed on to it here, over loopback, with a
//! short-lived token of the twin side's own server account.

use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::{Client, Method, header::CONTENT_TYPE, redirect::Policy};
use serde::Deserialize;
use tuwunel_core::{Err, Result, err, implement};

use super::{Service, read_secret};

/// Only the admin APIs are passed on.
const ALLOWED: [&str; 2] = ["/_synapse/admin/", "/_anoncat/admin/v1/"];

/// A token is asked for again this long before it runs out.
const RENEW_BEFORE: Duration = Duration::from_mins(10);

/// What the twin side answered.
pub struct Answer {
	pub status: u16,
	pub content_type: Option<String>,
	pub body: Bytes,
}

#[derive(Deserialize)]
struct Token {
	access_token: String,
	expires_in_ms: u64,
}

/// Whether a path may be passed on: under the admin APIs, with nothing that
/// could step outside them.
#[must_use]
fn allowed(path: &str) -> bool {
	ALLOWED.iter().any(|prefix| path.starts_with(prefix))
		&& !path.contains("..")
		&& !path.contains("//")
		&& !path.contains('\\')
		&& !path.starts_with("/_anoncat/admin/v1/twin")
}

impl Service {
	/// The twin side's server name, when this server keeps twins.
	#[must_use]
	pub fn twin_server(&self) -> Option<&ruma::ServerName> {
		self.is_source()
			.then(|| self.services.server.config.twin_server_name.as_deref())
			.flatten()
	}
}

/// Passes one admin request on to the twin side and returns its answer as
/// it is. `path_and_query` starts with `/`.
#[implement(Service)]
pub async fn panel_forward(&self, method: &str, path_and_query: &str, body: Bytes) -> Result<Answer> {
	let method = match method {
		| "GET" => Method::GET,
		| "POST" => Method::POST,
		| "PUT" => Method::PUT,
		| "DELETE" => Method::DELETE,
		| _ => return Err!(Request(Unrecognized("Method not allowed."))),
	};

	let path = path_and_query
		.split_once('?')
		.map_or(path_and_query, |(path, _)| path);

	if !self.is_source() || !allowed(path) {
		return Err!(Request(NotFound("Not found.")));
	}

	let token = self.panel_token_cached().await?;
	let url = self.twin_url(path_and_query)?;
	let mut request = self
		.panel_client()?
		.request(method, url)
		.bearer_auth(&token);

	if !body.is_empty() {
		request = request
			.header(CONTENT_TYPE, "application/json")
			.body(body);
	}

	let response = request
		.send()
		.await
		.map_err(|e| err!(Request(Unknown("The other anoncat server didn't answer: {}", e.without_url()))))?;

	let status = response.status().as_u16();
	if status == 401 {
		// Its token was replaced (it restarted with a new one): ask again
		// next time.
		*self.panel_token.lock().await = None;
	}

	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.map(ToOwned::to_owned);

	let body = response
		.bytes()
		.await
		.map_err(|e| err!(Request(Unknown("{}", e.without_url()))))?;

	Ok(Answer { status, content_type, body })
}

/// A token of the twin side's server account, asked for when there is none
/// or it runs out soon.
#[implement(Service)]
async fn panel_token_cached(&self) -> Result<String> {
	let mut cached = self.panel_token.lock().await;
	if let Some((token, until)) = cached.as_ref()
		&& until.saturating_duration_since(Instant::now()) > RENEW_BEFORE
	{
		return Ok(token.clone());
	}

	let config = &self.services.server.config;
	let Some(secret_file) = &config.twin_secret_file else {
		return Err!(Request(NotFound("Not found.")));
	};

	let secret = read_secret(secret_file).await?;
	let response = self
		.panel_client()?
		.post(self.twin_url("/_anoncat/twin/v1/panel-token")?)
		.bearer_auth(&secret)
		.header(CONTENT_TYPE, "application/json")
		.body("{}")
		.send()
		.await
		.map_err(|e| err!(Request(Unknown("The other anoncat server didn't answer: {}", e.without_url()))))?;

	if !response.status().is_success() {
		return Err!(Request(Unknown("The other anoncat server refused the panel: {}", response.status())));
	}

	let token: Token = response
		.json()
		.await
		.map_err(|e| err!(Request(Unknown("{}", e.without_url()))))?;

	let until = Instant::now() + Duration::from_millis(token.expires_in_ms);
	*cached = Some((token.access_token.clone(), until));

	Ok(token.access_token)
}

#[implement(Service)]
fn twin_url(&self, path_and_query: &str) -> Result<url::Url> {
	let Some(base) = &self.services.server.config.twin_api else {
		return Err!(Request(NotFound("Not found.")));
	};

	base.join(path_and_query)
		.map_err(|e| err!(Request(InvalidParam("{e}"))))
}

/// Loopback only (checked at startup), never through a proxy.
#[implement(Service)]
fn panel_client(&self) -> Result<&Client> {
	if let Some(client) = self.panel_client.get() {
		return Ok(client);
	}

	let client = Client::builder()
		.no_proxy()
		.redirect(Policy::none())
		.connect_timeout(Duration::from_secs(10))
		.timeout(Duration::from_mins(5))
		.build()?;

	Ok(self.panel_client.get_or_init(|| client))
}

#[cfg(test)]
mod tests {
	use super::allowed;

	#[test]
	fn only_admin_paths_go_through() {
		assert!(allowed("/_synapse/admin/v2/users"));
		assert!(allowed("/_anoncat/admin/v1/moderation/reports"));
		assert!(!allowed("/_matrix/client/v3/account/whoami"));
		assert!(!allowed("/_anoncat/twin/v1/panel-token"));
		assert!(!allowed("/_anoncat/admin/v1/twin/_synapse/admin/v2/users"));
		assert!(!allowed("/_synapse/admin/../../_matrix/client/v3/login"));
		assert!(!allowed("/_synapse/admin//x"));
		assert!(!allowed("/_synapse/admin\\x"));
	}
}
