use axum::extract::State;
use ruma::api::client::room::report_room;
use tuwunel_core::{Err, Result, info};
use tuwunel_service::moderation::Reported;

use super::REASON_MAX_LEN;
use crate::Ruma;

/// # `POST /_matrix/client/v3/rooms/{roomId}/report`
///
/// Reports an abusive room to homeserver admins
#[tracing::instrument(skip_all, name = "report_room")]
pub(crate) async fn report_room_route(
	State(services): State<crate::State>,
	body: Ruma<report_room::v3::Request>,
) -> Result<report_room::v3::Response> {
	let sender_user = body.sender_user();

	info!(
		"Received room report by user {sender_user} for room {} with reason: \"{}\"",
		body.room_id, body.reason,
	);

	if body.reason.len() > REASON_MAX_LEN {
		return Err!(Request(InvalidParam(
			"Reason too long, should be {REASON_MAX_LEN} characters or fewer"
		)));
	}

	if !services
		.state_cache
		.server_in_room(&services.server.name, &body.room_id)
		.await
	{
		return Err!(Request(NotFound(
			"Room does not exist to us, no local users have joined at all"
		)));
	}

	services
		.moderation
		.user_report(sender_user, Reported::Room { room_id: body.room_id.clone() }, &body.reason)
		.await;

	Ok(report_room::v3::Response {})
}
