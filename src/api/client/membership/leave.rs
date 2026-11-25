use axum::extract::State;
use futures::FutureExt;
use ruma::api::client::membership::leave_room::v3::{Request, Response};
use tuwunel_core::Result;

use crate::Ruma;

/// Leaves a room through `POST /_matrix/client/v3/rooms/{roomId}/leave`.
///
/// Notices rooms included: nobody is kept in a room they want out of.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn leave_room_route(
	State(services): State<crate::State>,
	Ruma { body, sender_user, .. }: Ruma<Request>,
) -> Result<Response> {
	let state_lock = services.state.mutex.lock(&body.room_id).await;
	let sender = sender_user
		.as_deref()
		.expect("user must be authenticated for this handler");

	services
		.membership
		.leave(sender, &body.room_id, body.reason, false, &state_lock)
		.await?;

	if services.config.delete_rooms_after_leave {
		services
			.delete
			.delete_if_empty_local(&body.room_id, state_lock)
			.boxed()
			.await;
	}

	Ok(Response {})
}
