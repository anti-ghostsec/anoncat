//! The twin side: making and updating twins as the source asks.

use futures::FutureExt;
use ruma::{OwnedServerName, OwnedUserId, RoomId, UserId, events::GlobalAccountDataEventType, push};
use tuwunel_core::{Err, Result, debug_warn, implement, info, utils::hash::is_phc_hash};

use super::{Account, AccountResult, Service};
use crate::{membership::Join, profile::Propagation};

/// Makes the twin when there is none (never for a deactivated account),
/// otherwise brings it in step: a new password hash replaces the old one, a
/// deactivated account is deactivated here too. A twin deactivated here is
/// never brought back.
#[implement(Service)]
pub async fn apply_account(&self, localpart: &str, account: Account) -> Result<AccountResult> {
	let user_id = self.twin_user_id(localpart)?;
	let services = &self.services;

	if let Some(hash) = &account.password_hash
		&& !is_phc_hash(hash)
	{
		return Err!(Request(InvalidParam("Not an Argon2 password hash.")));
	}

	if !services.users.exists(&user_id).await {
		if account.deactivated {
			return Ok(AccountResult::default());
		}

		self.create_twin(&user_id, &account).boxed().await?;
		let server_user: &UserId = &services.globals.server_user;
		if account.locked {
			services.users.set_locked(&user_id, server_user);
		}
		if account.suspended {
			services.users.set_suspended(&user_id, server_user);
		}

		return Ok(AccountResult { created: true, updated: false });
	}

	if services.users.is_deactivated(&user_id).await? {
		return Ok(AccountResult::default());
	}

	if account.deactivated {
		services
			.deactivate
			.full_deactivate(&user_id, false)
			.boxed()
			.await?;

		info!("Twin account deactivated");
		return Ok(AccountResult { created: false, updated: true });
	}

	let current_hash = services.users.password_hash(&user_id).await.ok();
	let new_hash = account
		.password_hash
		.as_ref()
		.filter(|hash| current_hash.as_ref() != Some(*hash));

	if let Some(hash) = new_hash {
		services
			.users
			.set_password_hash(&user_id, hash)
			.await?;
	}

	let current_name = services.profile.displayname(&user_id).await.ok();
	let new_name = account
		.displayname
		.as_ref()
		.filter(|name| current_name.as_ref() != Some(*name));

	if let Some(name) = new_name {
		services
			.profile
			.set_displayname(&user_id, Some(name), Some(Propagation::Unchanged))
			.boxed()
			.await?;
	}

	// Locked and suspended go with the person: a locked account can't be
	// used from its twin either.
	let server_user: &UserId = &services.globals.server_user;
	let lock_changed = services.users.is_locked(&user_id).await != account.locked;
	if lock_changed {
		if account.locked {
			services.users.set_locked(&user_id, server_user);
		} else {
			services.users.clear_locked(&user_id);
		}
	}

	let suspend_changed = services.users.is_suspended(&user_id).await != account.suspended;
	if suspend_changed {
		if account.suspended {
			services.users.set_suspended(&user_id, server_user);
		} else {
			services.users.clear_suspended(&user_id);
		}
	}

	let updated = new_hash.is_some() || new_name.is_some() || lock_changed || suspend_changed;
	Ok(AccountResult { created: false, updated })
}

/// Joins a twin to a room it was invited to; nothing to do when it is in
/// already.
#[implement(Service)]
pub async fn join_twin(&self, localpart: &str, room_id: &RoomId, via: &[OwnedServerName]) -> Result {
	let user_id = self.twin_user_id(localpart)?;
	let services = &self.services;

	if !services.users.is_active_local(&user_id).await {
		return Err!(Request(NotFound("No such twin here.")));
	}

	if services
		.state_cache
		.is_joined(&user_id, room_id)
		.await
	{
		return Ok(());
	}

	if !services
		.state_cache
		.is_invited(&user_id, room_id)
		.await
	{
		return Err!(Request(Forbidden("The twin isn't invited to that room.")));
	}

	services
		.membership
		.join(Join {
			sender_user: &user_id,
			room_id,
			orig_room_id: None,
			reason: None,
			servers: via,
			is_appservice: false,
			extra_content: None,
		})
		.boxed()
		.await
}

#[implement(Service)]
async fn create_twin(&self, user_id: &UserId, account: &Account) -> Result {
	let services = &self.services;

	services
		.users
		.create_with_hash(user_id, account.password_hash.as_deref())
		.await?;

	let displayname = account
		.displayname
		.as_deref()
		.unwrap_or_else(|| user_id.localpart());

	services
		.profile
		.set_displayname(user_id, Some(displayname), Some(Propagation::None))
		.await?;

	services
		.account_data
		.update(
			None,
			user_id,
			GlobalAccountDataEventType::PushRules
				.to_string()
				.into(),
			&serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
				content: ruma::events::push_rules::PushRulesEventContent {
					global: push::Ruleset::server_default(user_id),
				},
			})?,
		)
		.await?;

	// The source welcomes its own accounts; this server's welcome message
	// isn't for twins.
	services
		.notices
		.mark_welcomed(user_id.localpart())
		.await?;

	if let Some(text) = &account.welcome
		&& let Err(e) = services.notices.send(user_id, text).boxed().await
	{
		debug_warn!("Welcome message for a new twin not delivered: {e}");
	}

	info!("Twin account created");

	Ok(())
}

/// The twin's user ID here. Refused for a name that isn't a valid user ID
/// today, and for the server's own account.
#[implement(Service)]
fn twin_user_id(&self, localpart: &str) -> Result<OwnedUserId> {
	let services = &self.services;
	let Ok(user_id) = UserId::parse_with_server_name(localpart, services.globals.server_name())
	else {
		return Err!(Request(InvalidParam("Not a valid user name.")));
	};

	if user_id.localpart() != localpart || user_id.validate_strict().is_err() {
		return Err!(Request(InvalidParam("Not a valid user name.")));
	}

	if user_id == services.globals.server_user {
		return Err!(Request(Forbidden("The server's own account has no twin.")));
	}

	Ok(user_id)
}

/// The device the source's admin panel uses on this side.
const PANEL_DEVICE: &str = "ANONCATTWINPANEL";

/// How long a panel token lasts; the source asks again before then.
pub const PANEL_TOKEN_LIFETIME: std::time::Duration = std::time::Duration::from_hours(1);

/// A new token for this server's own account, for the source's admin panel:
/// the panel on the source runs this server too. The token before it stops
/// working.
#[implement(Service)]
pub async fn panel_token(&self) -> Result<String> {
	let services = &self.services;
	let server_user: &UserId = &services.globals.server_user;
	let device: &ruma::DeviceId = PANEL_DEVICE.into();
	let token = tuwunel_core::utils::random_string(64);

	// A server that never made its admin room has no account row for its
	// server account yet; made as the admin room would.
	if !services.users.exists(server_user).await {
		services
			.users
			.create(server_user, None, None)
			.await?;
		services
			.profile
			.set_displayname(server_user, Some("anoncat"), Some(Propagation::None))
			.await?;
	}

	if services.users.device_exists(server_user, device).await {
		services
			.users
			.set_access_token(server_user, device, &token, Some(PANEL_TOKEN_LIFETIME), None)
			.await?;
	} else {
		services
			.users
			.create_device(
				server_user,
				Some(device),
				(Some(&token), Some(PANEL_TOKEN_LIFETIME)),
				None,
				Some("anoncat panel (twin)"),
			)
			.await?;
	}

	Ok(token)
}
