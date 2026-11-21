//! `/_anoncat/admin/v1/announce`: a message from the server to its people,
//! sent from the admin panel.
//!
//! Each person gets it in their own notices room: a room only the server's
//! account can write in, tagged `m.server_notice`, so apps such as Element
//! keep it apart at the bottom of the room list. The room is made the first
//! time, with the person invited; later messages go to the same room. The
//! rooms never federate. The server's account stays deactivated: nobody can
//! sign in as it, the server writes these messages itself.

use axum::{Json, extract::State, http::HeaderMap};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId,
	events::{
		room::{
			create::RoomCreateEventContent,
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
		},
		tag::TagName,
	},
};
use serde::{Deserialize, Serialize};
use tuwunel_api_client::{notice_tag, room_is_notice};
use tuwunel_core::{
	Err, Result,
	utils::stream::{IterStream, ReadyExt},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

/// Longest message, in characters.
const MAX_TEXT: usize = 20_000;

/// How the server's account shows in the notices rooms.
const SENDER_NAME: &str = "anoncat";

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
		match send_to(&services, user, &content).boxed().await {
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

async fn send_to(
	services: &Services,
	target: &UserId,
	content: &RoomMessageEventContent,
) -> Result {
	let room_id = match find_notice_room(services, target).await {
		| Some(room_id) => room_id,
		| None => create_notice_room(services, target).await?,
	};

	let server_user: &UserId = &services.globals.server_user;
	let state_lock = services.state.mutex.lock(&room_id).await;

	// Invited again if they left; a pending invite stays as it is.
	let joined = services.state_cache.is_joined(target, &room_id).await;
	let invited = services.state_cache.is_invited(target, &room_id).await;
	if !joined && !invited {
		services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					target.as_str(),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				server_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	services
		.timeline
		.build_and_append_pdu(PduBuilder::timeline(content), server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	Ok(())
}

/// The person's notices room, if they were ever sent one.
async fn find_notice_room(services: &Services, target: &UserId) -> Option<OwnedRoomId> {
	let server_user: &UserId = &services.globals.server_user;
	let tag = notice_tag(&services.config.admin_room_tag);

	let candidates: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(target)
		.map(ToOwned::to_owned)
		.chain(
			services
				.state_cache
				.rooms_invited(target)
				.map(ToOwned::to_owned),
		)
		.chain(
			services
				.state_cache
				.rooms_left(target)
				.map(ToOwned::to_owned),
		)
		.collect()
		.await;

	for room_id in candidates {
		if is_notice(services, server_user, target, &tag, &room_id).await {
			return Some(room_id);
		}
	}

	None
}

async fn is_notice(
	services: &Services,
	server_user: &UserId,
	target: &UserId,
	tag: &TagName,
	room_id: &RoomId,
) -> bool {
	room_is_notice(services, server_user, target, tag, room_id)
		.await
		.is_ok_and(|notice| notice)
}

async fn create_notice_room(services: &Services, target: &UserId) -> Result<OwnedRoomId> {
	let room_id = RoomId::new_v1(services.globals.server_name());

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;
	let server_user: &UserId = &services.globals.server_user;

	// Kept on this server: other servers never see these rooms.
	let create = RoomCreateEventContent {
		federate: false,
		room_version: RoomVersionId::V11,
		..RoomCreateEventContent::new_v11()
	};

	let name = format!("Notices from {}", services.globals.server_name());

	[
		PduBuilder::state(String::new(), &create),
		PduBuilder::state(server_user.as_str(), &RoomMemberEventContent {
			displayname: Some(SENDER_NAME.to_owned()),
			..RoomMemberEventContent::new(MembershipState::Join)
		}),
		PduBuilder::state(String::new(), &notice_power_levels(server_user)),
		PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite)),
		PduBuilder::state(
			String::new(),
			&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Invited),
		),
		PduBuilder::state(String::new(), &RoomNameEventContent::new(name)),
	]
	.into_iter()
	.try_stream()
	.try_for_each(|pdu| {
		services
			.timeline
			.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
			.map_ok(|_| ())
	})
	.boxed()
	.await?;

	drop(state_lock);

	services
		.account_data
		.set_room_tag(target, &room_id, notice_tag(&services.config.admin_room_tag), None)
		.await?;

	Ok(room_id)
}

/// Only the server's account can write; the person can read and leave.
fn notice_power_levels(server_user: &UserId) -> RoomPowerLevelsEventContent {
	RoomPowerLevelsEventContent {
		users: [(server_user.into(), 100.into())].into(),
		users_default: (-10).into(),
		..Default::default()
	}
}

#[cfg(test)]
mod tests {
	use ruma::{Int, user_id};

	use super::notice_power_levels;

	#[test]
	fn people_cant_write_in_notices_rooms() {
		let server = user_id!("@server:example.com");
		let power_levels = notice_power_levels(server);

		assert_eq!(power_levels.users.get(server), Some(&Int::from(100)));
		assert!(power_levels.users_default < power_levels.events_default);
	}
}
