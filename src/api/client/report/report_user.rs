use axum::extract::State;
use ruma::api::client::reporting::report_user;
use tuwunel_core::{Err, Result, info};
use tuwunel_service::moderation::Reported;

use super::REASON_MAX_LEN;
use crate::Ruma;

/// # `POST /_matrix/client/v3/users/{userId}/report`
///
/// Reports an inappropriate user to homeserver admins (MSC4260).
#[tracing::instrument(skip_all, name = "report_user")]
pub(crate) async fn report_user_route(
	State(services): State<crate::State>,
	body: Ruma<report_user::v3::Request>,
) -> Result<report_user::v3::Response> {
	let sender_user = body.sender_user();
	let target_user = &body.user_id;
	let reason = &body.reason;

	if reason.len() > REASON_MAX_LEN {
		return Err!(Request(InvalidParam(
			"Reason too long, should be {REASON_MAX_LEN} characters or fewer"
		)));
	}

	info!("Received user report by {sender_user} for {target_user} with reason: \"{reason}\"");

	// Succeed regardless of user existence to deter enumeration (MSC4277).
	// Someone from another server can be reported too: they may be the one
	// messaging people here.
	let known = services.users.is_active_local(target_user).await
		|| !services.globals.user_is_local(target_user);
	if known {
		services
			.moderation
			.user_report(sender_user, Reported::User { user_id: target_user.clone() }, reason)
			.await;
	}

	Ok(report_user::v3::Response {})
}
