//! `/_anoncat/twin/v1/...`: the twin side of a twin pair (see the `twins`
//! service), for the source server on the same machine. Served only on the
//! loopback panel listener, only with `twin_of` and `twin_secret_file` set,
//! and only to requests carrying the shared secret.

use axum::{
	Json,
	body::Bytes,
	extract::{Path, State},
	http::{HeaderMap, header::AUTHORIZATION},
};
use serde::{Serialize, de::DeserializeOwned};
use tuwunel_core::{Err, Error, Result, err};
use tuwunel_service::twins::{Account, AccountResult, JoinRequest};

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
