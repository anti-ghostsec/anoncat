//! Messages from the server to one person, in their notices room.
//!
//! Each person gets them in their own notices room: a room only the server's
//! account can write in, tagged `m.server_notice`, so apps such as Element
//! keep it apart at the bottom of the room list. The room is made the first
//! time, with the person invited; later messages go to the same room. The
//! rooms never federate. The server's account stays deactivated: nobody can
//! sign in as it, the server writes these messages itself.
//!
//! The panel's announcements go out this way, and so does the welcome
//! message new accounts get (see [`welcome`]).

mod welcome;

use std::sync::Arc;

use async_trait::async_trait;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::ready};
use ruma::{
	OwnedRoomId, RoomId, RoomVersionId, UserId,
	events::{
		StateEventType,
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
use tuwunel_core::{
	Result, implement,
	utils::{BoolExt, stream::IterStream},
};
use tuwunel_matrix::{Event, pdu::PduBuilder};

pub use self::welcome::MAX_WELCOME;

/// How the server's account shows in the notices rooms.
const SENDER_NAME: &str = "anoncat";

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	settings: Arc<tuwunel_database::Map>,
	/// Held while the welcome settings are read and written back.
	welcome_lock: tokio::sync::Mutex<()>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			settings: args.db["anoncat_settings"].clone(),
			welcome_lock: tokio::sync::Mutex::new(()),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result { self.welcome_worker().await }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// The server's account shows as "anoncat" in apps that look at its profile
/// rather than its name in the room. Its user ID can't change after the
/// first start; this is the name people see.
#[implement(Service)]
pub(super) async fn name_server_account(&self) {
	let services = &self.services;
	let server_user: &UserId = &services.globals.server_user;
	if services.profile.displayname(server_user).await.ok().as_deref() == Some(SENDER_NAME) {
		return;
	}

	if let Err(e) = services
		.profile
		.set_displayname(server_user, Some(SENDER_NAME), Some(crate::profile::Propagation::None))
		.await
	{
		tuwunel_core::debug_warn!("Couldn't name the server's account: {e}");
	}
}

/// Sends Markdown, as people would type it in their app, to one person.
/// `{name}` in it becomes the person's user name (the part before the
/// colon), so a message can show each person their own accounts.
#[implement(Service)]
pub async fn send(&self, target: &UserId, markdown: &str) -> Result {
	let markdown = personalize(markdown, target);
	self.send_content(target, &RoomMessageEventContent::text_markdown(&markdown))
		.await
}

/// `{name}` replaced by the person's user name.
#[must_use]
pub fn personalize(markdown: &str, target: &UserId) -> String {
	markdown.replace("{name}", target.localpart())
}

/// Sends a message to one person, making their notices room the first time
/// and putting them in it (again, if they left).
#[implement(Service)]
pub async fn send_content(&self, target: &UserId, content: &RoomMessageEventContent) -> Result {
	let services = &self.services;
	let room_id = match self.find_notice_room(target).await {
		| Some(room_id) => room_id,
		| None => self.create_notice_room(target).boxed().await?,
	};

	let server_user: &UserId = &services.globals.server_user;
	let state_lock = services.state.mutex.lock(&room_id).await;

	// Put straight in the first time, so the message is simply there in their
	// room list (an invite from a server account looks like spam). Someone who
	// left, or declined, is only invited again: they can leave for good.
	let membership = services
		.state_accessor
		.get_member(&room_id, target)
		.await
		.ok()
		.map(|member| member.membership);

	if !matches!(membership, Some(MembershipState::Join | MembershipState::Invite)) {
		services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(target.as_str(), &RoomMemberEventContent::new(MembershipState::Invite)),
				server_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	if puts_in(membership.as_ref()) {
		services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(target.as_str(), &RoomMemberEventContent::new(MembershipState::Join)),
				target,
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

/// Whether a message puts the person in their notices room: the first time,
/// and while an invite from before is still waiting, but never after they
/// left or declined.
fn puts_in(membership: Option<&MembershipState>) -> bool {
	matches!(membership, None | Some(MembershipState::Invite))
}

/// Tests whether a room is the target user's server-notice room.
///
/// The configured tag and global server identity are applied consistently with
/// notice-room lookup and creation.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn is_notice_room(&self, target: &UserId, room_id: &RoomId) -> Result<bool> {
	let services = &self.services;
	let server_user = services.globals.server_user.as_ref();
	let tag = notice_tag(&services.config.admin_room_tag);

	if services
		.admin
		.get_admin_room()
		.map(|admin| admin.is_ok_and(|admin| admin == room_id))
		.await
	{
		return Ok(false);
	}

	self.room_is_notice(server_user, target, &tag, room_id)
		.await
}

/// Checks server membership, the target's tag, and the immutable room creator.
///
/// An absent tag is an ordinary non-notice room; other lookup failures propagate.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn room_is_notice(
	&self,
	server_user: &UserId,
	target: &UserId,
	tag: &TagName,
	room_id: &RoomId,
) -> Result<bool> {
	let services = &self.services;
	if !services
		.state_cache
		.is_joined(server_user, room_id)
		.await
	{
		return Ok(false);
	}

	let tagged = services
		.account_data
		.get_room_tags(target, room_id)
		.map_ok(|tags| tags.contains_key(tag))
		.or_else(|error| ready(error.is_not_found().then_ok_or(false, error)))
		.await?;

	if !tagged {
		return Ok(false);
	}

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map_ok(|create| is_notice_creator(create.sender(), server_user))
		.await
}

/// The person's notices room, if they were ever sent one.
#[implement(Service)]
async fn find_notice_room(&self, target: &UserId) -> Option<OwnedRoomId> {
	let services = &self.services;
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
		if self
			.room_is_notice(server_user, target, &tag, &room_id)
			.await
			.is_ok_and(|notice| notice)
		{
			return Some(room_id);
		}
	}

	None
}

#[implement(Service)]
async fn create_notice_room(&self, target: &UserId) -> Result<OwnedRoomId> {
	let services = &self.services;
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

/// Selects the configured notice tag, falling back when it is empty.
///
/// The fallback matches the tag used when creating server-notice rooms.
#[must_use]
pub fn notice_tag(tag: &str) -> TagName {
	Some(tag)
		.filter(|tag| !tag.is_empty())
		.map_or(TagName::ServerNotice, Into::into)
}

/// Matches the immutable create-event sender to the configured server identity.
///
/// Membership alone cannot distinguish notice rooms from ordinary shared rooms.
fn is_notice_creator(create_sender: &UserId, server_user: &UserId) -> bool {
	create_sender == server_user
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
	use ruma::{Int, events::tag::TagName, user_id};

	use ruma::events::room::member::MembershipState;

	use super::{is_notice_creator, notice_power_levels, notice_tag, personalize, puts_in};

	#[test]
	fn people_who_left_are_only_invited() {
		assert!(puts_in(None));
		assert!(puts_in(Some(&MembershipState::Invite)));
		assert!(!puts_in(Some(&MembershipState::Leave)));
		assert!(!puts_in(Some(&MembershipState::Join)));
	}

	#[test]
	fn each_person_sees_their_own_name() {
		let user = user_id!("@cat:example.com");
		assert_eq!(personalize("`@{name}:a` and `@{name}:b`", user), "`@cat:a` and `@cat:b`");
		assert_eq!(personalize("no name here", user), "no name here");
	}

	#[test]
	fn people_cant_write_in_notices_rooms() {
		let server = user_id!("@server:example.com");
		let power_levels = notice_power_levels(server);

		assert_eq!(power_levels.users.get(server), Some(&Int::from(100)));
		assert!(power_levels.users_default < power_levels.events_default);
	}

	#[test]
	fn only_the_server_user_creates_a_notice_room() {
		let server = user_id!("@server:example.com");
		let other = user_id!("@other:example.com");

		assert!(is_notice_creator(server, server));
		assert!(!is_notice_creator(other, server));
	}

	#[test]
	fn empty_config_tag_falls_back_to_server_notice() {
		assert_eq!(notice_tag(""), TagName::ServerNotice);
		assert_eq!(notice_tag("m.server_notice"), TagName::ServerNotice);
		assert_eq!(notice_tag("u.custom"), TagName::from("u.custom"));
	}
}
