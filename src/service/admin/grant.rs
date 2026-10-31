use ruma::{
	Int, OwnedUserId, RoomId, UserId,
	events::{
		StateEventType,
		room::{
			power_levels::{RoomPowerLevelsEventContent, UserPowerLevel},
		},
	},
};
use tuwunel_core::{
	Err, Result, debug_info, implement,
	utils::{FutureBoolExt, future::ReadyBoolExt, stream::ReadyExt},
};
use tuwunel_matrix::pdu::PduBuilder;

/// Makes a local user a server admin.
///
/// anoncat keeps admin rights as a flag on the account, with no admin room or
/// bot. A local user must already hold an account.
#[implement(super::Service)]
pub async fn make_user_admin(&self, user_id: &UserId) -> Result {
	if !self.services.globals.user_is_local(user_id) {
		return Err!(Request(InvalidParam("Only users of this server can be admins.")));
	}

	if !self.services.users.exists(user_id).await {
		return Err!(Request(NotFound("User {user_id} does not exist on this server.")));
	}

	let _lock = self.lock_admin_room().await;
	self.admins.insert(user_id, []);
	debug_info!(%user_id, "Granted admin rights");

	Ok(())
}

/// Grant room admin powers to a target by impersonating the highest-powered
/// local member.
///
/// The target is granted the impersonated member's power level (100 when that
/// member is a room-version 12 creator with infinite power), sent as a new
/// `m.room.power_levels` from that member. When the room is non-public and the
/// target is neither joined nor invited, the target is invited first.
#[implement(super::Service)]
pub async fn make_room_admin(&self, room_id: &RoomId, target: &UserId) -> Result {
	let state_lock = self.services.state.mutex.lock(room_id).await;

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(room_id)
		.await?;

	let sender = self
		.services
		.state_cache
		.local_users_in_room(room_id)
		.ready_fold(None, |best: Option<(OwnedUserId, UserPowerLevel)>, user| {
			let level = power_levels.for_user(user);
			match best {
				| Some((_, best_level)) if best_level >= level => best,
				| _ => Some((user.to_owned(), level)),
			}
		})
		.await;

	let Some((sender, sender_level)) = sender else {
		return Err!(Request(InvalidParam("Server not in room")));
	};

	if !power_levels.user_can_send_state(&sender, StateEventType::RoomPowerLevels) {
		return Err!(Request(InvalidParam(
			"No local admin in room with power to update power levels"
		)));
	}

	let grant_level: Int = match sender_level {
		| UserPowerLevel::Infinite => 100.into(),
		| UserPowerLevel::Int(level) => level,
	};

	let is_joined = self
		.services
		.state_cache
		.is_joined(target, room_id);

	let is_invited = self
		.services
		.state_cache
		.is_invited(target, room_id);

	let is_public = self.services.metadata.is_public(room_id);

	// The membership probes lead; the public check is the multi-read leg.
	let needs_invite = is_joined
		.is_false()
		.and2(is_invited.is_false(), is_public.is_false());

	if needs_invite.await {
		self.services
			.membership
			.invite(&sender, target, room_id, None, false)
			.await?;
	}

	let mut content = self
		.services
		.state_accessor
		.room_state_get_content::<RoomPowerLevelsEventContent>(
			room_id,
			&StateEventType::RoomPowerLevels,
			"",
		)
		.await
		.unwrap_or_default();

	content.users.insert(target.into(), grant_level);

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &content),
			&sender,
			room_id,
			&state_lock,
		)
		.await
		.map(|_| ())
}


/// Demote an admin, removing its rights.
///
/// Returns whether there was a grant to revoke. The server user is refused.
/// The last admin who can still sign in is kept, since nobody could grant the
/// rights again.
#[implement(super::Service)]
pub async fn revoke_admin(&self, user_id: &UserId) -> Result<bool> {
	if user_id == self.services.globals.server_user {
		return Err!(Request(InvalidParam("The server user's admin rights cannot be revoked.")));
	}

	let _lock = self.lock_admin_room().await;
	if !self.user_is_admin(user_id).await {
		return Ok(false);
	}

	if self.user_is_last_admin(user_id).await {
		return Err!(Request(Forbidden(
			"{user_id} is the last admin; make someone else an admin first."
		)));
	}

	self.admins.remove(user_id);
	debug_info!(%user_id, "Revoked admin rights");

	Ok(true)
}
