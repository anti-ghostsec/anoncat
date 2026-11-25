//! `/_anoncat/twin/v1/...`: the twin side of a twin pair (see the `twins`
//! service), for the source server on the same machine. Served only on the
//! loopback panel listener, only with `twin_of` and `twin_secret_file` set,
//! and only to requests carrying the shared secret.

use axum::{
	Json,
	body::{Body, Bytes},
	extract::{Path, State},
	http::{
		HeaderMap, HeaderValue, Method, StatusCode, Uri,
		header::{AUTHORIZATION, CONTENT_TYPE},
	},
	response::Response,
};
use serde::{Serialize, de::DeserializeOwned};
use tuwunel_core::{Err, Error, Result, err};
use tuwunel_service::twins::{Account, AccountResult, JoinRequest, Mirror, MirrorAnswer, MirrorState};

#[derive(Serialize)]
pub(crate) struct Joined {
	joined: bool,
}

/// `PUT /_anoncat/twin/v1/users/{localpart}`: makes or updates a twin.
pub(crate) async fn put_user_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(localpart): Path<String>,
	body: Bytes,
) -> Result<Json<AccountResult>> {
	let account: Account = authorized_body(&services, &headers, &body).await?;

	services
		.twins
		.apply_account(&localpart, account)
		.await
		.map(Json)
}

/// `POST /_anoncat/twin/v1/users/{localpart}/join`: joins a twin to a room
/// it was invited to.
pub(crate) async fn join_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(localpart): Path<String>,
	body: Bytes,
) -> Result<Json<Joined>> {
	let request: JoinRequest = authorized_body(&services, &headers, &body).await?;

	services
		.twins
		.join_twin(&localpart, &request.room_id, &request.via)
		.await?;

	Ok(Json(Joined { joined: true }))
}

#[derive(Serialize)]
pub(crate) struct PanelToken {
	access_token: String,
	expires_in_ms: u64,
}

/// `POST /_anoncat/twin/v1/panel-token`: a short-lived token of this
/// server's own account, for the source's admin panel.
pub(crate) async fn panel_token_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Json<PanelToken>> {
	let _: serde_json::Value = authorized_body(&services, &headers, &body).await?;
	let access_token = services.twins.panel_token().await?;
	let lifetime = tuwunel_service::twins::PANEL_TOKEN_LIFETIME;

	Ok(Json(PanelToken {
		access_token,
		expires_in_ms: u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX),
	}))
}

/// `/_anoncat/admin/v1/twin/{path}`: the panel's requests for the other
/// anoncat server (the twin side), passed on to it as they are. Only the
/// admin APIs, and only for this server's admins.
pub(crate) async fn forward_route(
	State(services): State<crate::State>,
	method: Method,
	uri: Uri,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Response> {
	// The path as it came, still percent-encoded, so IDs pass unchanged.
	let Some(rest) = uri.path().strip_prefix("/_anoncat/admin/v1/twin") else {
		return Err!(Request(NotFound("Not found.")));
	};

	// A play link is its own key, as on the other server itself: a video
	// player can't send the panel's token.
	let play_link = method == Method::GET && rest.starts_with("/_anoncat/admin/v1/moderation/play/");
	if !play_link {
		super::require_admin_token(&services, &headers).await?;
	}

	let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
	let content_type = headers
		.get(CONTENT_TYPE)
		.and_then(|value| value.to_str().ok());

	let range = headers
		.get(axum::http::header::RANGE)
		.and_then(|value| value.to_str().ok());

	let answer = services
		.twins
		.panel_forward(method.as_str(), &format!("{rest}{query}"), content_type, range, body)
		.await?;

	let mut response = Response::new(Body::from(answer.body));
	*response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
	if let Some(value) = answer
		.content_type
		.and_then(|value| HeaderValue::from_str(&value).ok())
	{
		response.headers_mut().insert(CONTENT_TYPE, value);
	}

	for (name, value) in answer.headers {
		if let (Ok(name), Ok(value)) = (
			axum::http::HeaderName::from_bytes(name.as_bytes()),
			HeaderValue::from_str(&value),
		) {
			response.headers_mut().insert(name, value);
		}
	}

	Ok(response)
}

/// `GET /_anoncat/twin/v1/moderation`: this side's bad-files count.
pub(crate) async fn moderation_state_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<MirrorState>> {
	let _: serde_json::Value = authorized_body(&services, &headers, b"null").await?;
	Ok(Json(services.twins.moderation_state()))
}

/// `PUT /_anoncat/twin/v1/moderation`: the source's moderation settings,
/// AI key and known bad files.
pub(crate) async fn moderation_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Json<MirrorAnswer>> {
	let mirror: Mirror = authorized_body(&services, &headers, &body).await?;
	services
		.twins
		.apply_moderation(mirror)
		.await
		.map(Json)
}

/// The body, once the request is known to come from the source. Nothing is
/// read from it before.
async fn authorized_body<T: DeserializeOwned>(
	services: &crate::State,
	headers: &HeaderMap,
	body: &[u8],
) -> Result<T> {
	if !services.twins.is_twin() {
		return Err!(Request(NotFound("Not found.")));
	}

	let given = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default();

	if !services.twins.secret_matches(given.trim()).await {
		return Err(Error::unknown_token(false, "Wrong or missing twin secret."));
	}

	serde_json::from_slice(body).map_err(|e| err!(Request(BadJson("{e}"))))
}
