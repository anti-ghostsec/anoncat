//! `/_anoncat/admin/v1/announce`: a message from the server to its people,
//! sent from the admin panel. Each person gets it in their own notices room
//! (see the `notices` service).

use axum::{Json, extract::State, http::HeaderMap};
use futures::{FutureExt, StreamExt};
use ruma::{OwnedUserId, UserId, events::room::message::RoomMessageEventContent};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, utils::stream::ReadyExt};
use tuwunel_service::Services;

/// Longest message, in characters.
const MAX_TEXT: usize = 20_000;

#[derive(Deserialize)]
pub(crate) struct Request {
	/// Markdown, as people would type it in their app.
	text: String,
	/// One person only, to see how it looks before sending it to everyone.
	#[serde(default)]
	to: Option<OwnedUserId>,
}

#[derive(Serialize)]
pub(crate) struct Response {
	sent: usize,
	failed: Vec<Failed>,
}

#[derive(Serialize)]
struct Failed {
	user: String,
	error: String,
}

#[derive(Serialize)]
pub(crate) struct Recipients {
	/// Accounts here that can sign in: who "everyone" reaches.
	people: usize,
}

pub(crate) async fn recipients_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Recipients>> {
	super::require_admin_token(&services, &headers).await?;

	Ok(Json(Recipients { people: recipients(&services).await.len() }))
}

pub(crate) async fn announce_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<Request>,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;

	let text = body.text.trim();
	if text.is_empty() {
		return Err!(Request(InvalidParam("The message is empty.")));
	}
	if text.chars().count() > MAX_TEXT {
		return Err!(Request(InvalidParam("The message is longer than {MAX_TEXT} characters.")));
	}

	let targets = match body.to {
		| Some(user) => {
			if !services.globals.user_is_local(&user) || !services.users.is_active_local(&user).await
			{
				return Err!(Request(NotFound("No account here can sign in as {user}.")));
			}
			vec![user]
		},
		| None => recipients(&services).await,
	};

	let content = RoomMessageEventContent::text_markdown(text);
	let mut sent = 0_usize;
	let mut failed = Vec::new();
	for user in &targets {
		match services.notices.send_content(user, &content).boxed().await {
			| Ok(()) => sent = sent.saturating_add(1),
			| Err(e) => failed.push(Failed { user: user.to_string(), error: e.sanitized_message() }),
		}
	}

	Ok(Json(Response { sent, failed }))
}

/// Everyone with an account here that can sign in, but the server's own.
async fn recipients(services: &Services) -> Vec<OwnedUserId> {
	let server_user: &UserId = &services.globals.server_user;

	services
		.users
		.list_local_users()
		.ready_filter(|user| *user != server_user)
		.map(ToOwned::to_owned)
		.collect()
		.await
}
