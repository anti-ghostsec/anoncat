use std::str;

use axum::{RequestPartsExt, extract::Path};
use axum_extra::extract::cookie::CookieJar;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http::{StatusCode, request::Parts};
use ruma::api::error::{ErrorKind, LimitExceededErrorData, RetryAfter};
use serde::Deserialize;
use tuwunel_core::{
	Error, Result, err, smallstr::SmallString, smallvec::SmallVec,
	utils::memory_budget::Reservation,
};
use tuwunel_service::{Services, media::spool::Spooled};

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
	/// Room held in the budget for big files, while the upload is handled.
	pub(super) reservation: Option<Reservation>,
	/// A big upload, written to disk as it arrived; `body` is then empty.
	pub(super) spooled: Option<Spooled>,
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
	// account can make the server hold more than `max_request_size`. Such an
	// upload also takes room in the server's budget for big files as it
	// arrives, so many at once can't run the server out of memory.
	let config = &services.server.config;
	let owner = if config.upload_limit() > config.max_request_size && is_upload(parts.uri.path())
	{
		token_owner(services, &parts, &query).await
	} else {
		None
	};

	let (body, reservation, spooled) = match owner {
		| Some(owner) => {
			let reservation = services.server.big_bodies.reserve(&owner);
			read_reserved(services, body, &parts, config.upload_limit(), reservation).await?
		},
		| None => {
			let body = axum::body::to_bytes(body, config.max_request_size)
				.await
				.map_err(|e| err!(Request(TooLarge("Request body too large: {e}"))))?;

			(body, None, None)
		},
	};

	Ok(Request { cookie, path, query, body, parts, reservation, spooled })
}

/// Reads an upload, growing its reservation as the bytes arrive. Refused at
/// once when the budget, or this account's share of it, is used up.
///
/// Past the media service's `spool_over` the upload goes to disk instead
/// (what had arrived, then the rest as it comes), and its reservation
/// shrinks back: a 2 GiB video costs disk, not memory.
async fn read_reserved(
	services: &Services,
	body: axum::body::Body,
	parts: &Parts,
	limit: usize,
	mut reservation: Reservation,
) -> Result<(Bytes, Option<Reservation>, Option<Spooled>)> {
	let too_large = || err!(Request(TooLarge("Request body too large")));

	let declared = parts
		.headers
		.get(http::header::CONTENT_LENGTH)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<usize>().ok());

	if declared.is_some_and(|len| len > limit) {
		return Err(too_large());
	}

	let spool_over = services.media.spool_over();
	let mut spool = match declared {
		| Some(len) if len > spool_over => Some(services.media.spool(len as u64).await?),
		| _ => None,
	};

	if spool.is_none() && !reservation.grow_to(declared.unwrap_or(0)) {
		return Err(busy());
	}

	let mut buf = BytesMut::with_capacity(if spool.is_some() { 0 } else { declared.unwrap_or(0) });
	let mut stream = body.into_data_stream();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.map_err(|e| err!(Request(Unknown("Failed to read the body: {e}"))))?;
		if let Some(spool) = spool.as_mut() {
			let len = usize::try_from(spool.len())
				.unwrap_or(usize::MAX)
				.saturating_add(chunk.len());
			if len > limit {
				return Err(too_large());
			}
			spool.write(&chunk).await?;
			continue;
		}

		let len = buf.len().saturating_add(chunk.len());
		if len > limit {
			return Err(too_large());
		}

		// Sent without a length, and now too big to keep in memory.
		if len > spool_over {
			let mut to_disk = services.media.spool(len as u64).await?;
			to_disk.write(&buf).await?;
			to_disk.write(&chunk).await?;
			buf = BytesMut::new();
			spool = Some(to_disk);
			continue;
		}

		if !reservation.grow_to(len) {
			return Err(busy());
		}

		buf.extend_from_slice(&chunk);
	}

	match spool {
		| Some(spool) => {
			drop(reservation);
			Ok((Bytes::new(), None, Some(spool.finish().await?)))
		},
		| None => Ok((buf.freeze(), Some(reservation), None)),
	}
}

fn busy() -> Error {
	Error::Request(
		ErrorKind::LimitExceeded(LimitExceededErrorData {
			retry_after: Some(RetryAfter::Delay(Duration::from_secs(30))),
		}),
		"The server is busy with other big files. Try again shortly.".into(),
		StatusCode::TOO_MANY_REQUESTS,
	)
}


/// Media upload routes: `/_matrix/media/{version}/upload[/{server}/{id}]`.
fn is_upload(path: &str) -> bool {
	let mut segments = path.split('/').skip(1);
	segments.next() == Some("_matrix")
		&& segments.next() == Some("media")
		&& segments.next().is_some()
		&& segments.next() == Some("upload")
}

/// Who an upload belongs to, when it carries an access token of an account or
/// appservice here, by header or query, that hasn't expired.
async fn token_owner(services: &Services, parts: &Parts, query: &QueryParams) -> Option<String> {
	let header = parts
		.headers
		.get(http::header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.map(str::trim);
	let token = header.or(query.access_token.as_deref())?;

	if let Ok((user, _, expires_at)) = services.users.find_from_token(token).await {
		return expires_at
			.is_none_or(|at| at > std::time::SystemTime::now())
			.then(|| user.to_string());
	}

	services
		.appservice
		.find_from_access_token(token)
		.await
		.ok()
		.map(|info| format!("appservice {}", info.registration.id))
}
