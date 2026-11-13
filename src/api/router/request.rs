use std::str;

use axum::{RequestPartsExt, extract::Path};
use axum_extra::extract::cookie::CookieJar;
use bytes::Bytes;
use http::request::Parts;
use serde::Deserialize;
use tuwunel_core::{Result, err, smallstr::SmallString, smallvec::SmallVec};
use tuwunel_service::Services;

#[derive(Debug, Deserialize)]
pub(super) struct QueryParams {
	pub(super) access_token: Option<String>,

	pub(super) user_id: Option<UserId>,

	pub(super) device_id: Option<DeviceId>,

	#[serde(rename = "org.matrix.msc3202.device_id")]
	pub(super) msc3202_device_id: Option<DeviceId>,
}

impl QueryParams {
	pub(super) fn device_id(&self) -> Option<&str> {
		self.device_id
			.as_deref()
			.or(self.msc3202_device_id.as_deref())
	}
}

pub(super) type UserId = SmallString<[u8; 48]>;
pub(super) type DeviceId = SmallString<[u8; 24]>;

#[derive(Debug)]
pub(super) struct Request {
	pub(super) cookie: CookieJar,
	pub(super) path: Path<PathParams>,
	pub(super) query: QueryParams,
	pub(super) body: Bytes,
	pub(super) parts: Parts,
}

pub(super) type PathParams = SmallVec<[PathParam; 8]>;
pub(super) type PathParam = SmallString<[u8; 32]>;

#[tracing::instrument(
	name = "parse",
	level = "trace",
	skip_all,
	err(level = "debug")
)]
pub(super) async fn from(
	services: &Services,
	request: http::Request<axum::body::Body>,
) -> Result<Request> {
	let (mut parts, body) = request.into_parts();

	let cookie: CookieJar = parts.extract().await?;
	let path: Path<PathParams> = parts.extract().await?;

	let query = parts.uri.query().unwrap_or_default();
	let query = serde_html_form::from_str(query)
		.map_err(|e| err!(Request(Unknown("Failed to read query parameters: {e}"))))?;

	// Bodies are held in memory, and read before the request's
	// authentication is checked. So the bigger upload limit is only for an
	// upload whose access token is valid, checked first: nobody without an
	// account can make the server hold more than `max_request_size`.
	let config = &services.server.config;
	let max_body_size = if config.upload_limit() > config.max_request_size
		&& is_upload(parts.uri.path())
		&& has_valid_token(services, &parts, &query).await
	{
		config.upload_limit()
	} else {
		config.max_request_size
	};
	let body = axum::body::to_bytes(body, max_body_size)
		.await
		.map_err(|e| err!(Request(TooLarge("Request body too large: {e}"))))?;

	Ok(Request { cookie, path, query, body, parts })
}

/// Media upload routes: `/_matrix/media/{version}/upload[/{server}/{id}]`.
fn is_upload(path: &str) -> bool {
	let mut segments = path.split('/').skip(1);
	segments.next() == Some("_matrix")
		&& segments.next() == Some("media")
		&& segments.next().is_some()
		&& segments.next() == Some("upload")
}

/// Whether the request carries an access token of an account or appservice
/// here, by header or query, that hasn't expired.
async fn has_valid_token(services: &Services, parts: &Parts, query: &QueryParams) -> bool {
	let header = parts
		.headers
		.get(http::header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.map(str::trim);
	let Some(token) = header.or(query.access_token.as_deref()) else {
		return false;
	};

	if let Ok((_, _, expires_at)) = services.users.find_from_token(token).await {
		return expires_at.is_none_or(|at| at > std::time::SystemTime::now());
	}

	services
		.appservice
		.find_from_access_token(token)
		.await
		.is_ok()
}
