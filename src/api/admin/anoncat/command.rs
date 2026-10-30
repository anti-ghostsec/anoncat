//! `POST /_anoncat/admin/v1/command`: runs an admin command, as typed in the
//! admin room, and returns its output.

use axum::{Json, extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;
use tuwunel_service::admin::CommandInput;

#[derive(Deserialize)]
pub(crate) struct Request {
	/// The command without the `!admin` prefix, e.g. "server cleanup".
	command: String,
}

#[derive(Serialize)]
pub(crate) struct Response {
	/// Whether the command succeeded.
	ok: bool,
	/// What the command printed (Markdown or plain text).
	output: String,
}

pub(crate) async fn anoncat_command_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<Request>,
) -> Result<Json<Response>> {
	let sender = super::require_admin_token(&services, &headers).await?;
	let input = CommandInput {
		command: body.command,
		reply_id: None,
		sender: Some(sender),
	};

	let (ok, output) = match services.admin.command_in_place(input).await {
		| Ok(output) => (true, output.map(|o| o.as_str().to_owned()).unwrap_or_default()),
		| Err(output) => (false, output.as_str().to_owned()),
	};

	Ok(Json(Response { ok, output }))
}
