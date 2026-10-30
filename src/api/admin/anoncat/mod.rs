//! anoncat's own admin endpoints, used by the admin panel.
//!
//! Plain axum handlers: ruma's request macros can't read this crate's
//! manifest, so authentication is checked here directly.

pub(crate) mod command;
pub(crate) mod status;

use axum::http::{HeaderMap, header::AUTHORIZATION};
use ruma::OwnedUserId;
use tuwunel_core::{Err, Error, Result, utils::time::timepoint_has_passed};

pub(crate) use self::{command::anoncat_command_route, status::anoncat_status_route};

/// The server admin behind the request's access token.
async fn require_admin_token(services: &crate::State, headers: &HeaderMap) -> Result<OwnedUserId> {
	let Some(token) = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
	else {
		return Err!(Request(MissingToken("Missing access token.")));
	};

	let Ok((user_id, _device, expires)) = services.users.find_from_token(token.trim()).await
	else {
		return Err(Error::unknown_token(false, "Unknown access token."));
	};

	if expires.is_some_and(timepoint_has_passed) {
		return Err(Error::unknown_token(true, "Access token has expired."));
	}

	// A locked account is shut out here as on every other route.
	services.users.locked_check(&user_id).await?;
	super::require_admin(services, &user_id).await?;

	Ok(user_id)
}
