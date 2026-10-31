use futures::StreamExt;
use tuwunel_core::{Result, info, warn};

use crate::Services;

pub(super) const MARKER: &str = "anoncat_no_bot";

/// Retires the admin room and its bot on a server that has them.
///
/// The room's human members keep their admin rights as a flag on their
/// accounts; then the room is deleted (everyone local made to leave, its
/// aliases removed, its history purged) and the bot account is deactivated.
/// anoncat's admins work from the panel instead.
pub(super) async fn no_bot(services: &Services) -> Result {
	let server_user = services.globals.server_user.as_ref();

	if let Ok(room_id) = services.admin.get_admin_room().await {
		let members: Vec<_> = services
			.state_cache
			.active_local_users_in_room(&room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for user_id in members.iter().filter(|user_id| *user_id != server_user) {
			services.admin.make_user_admin(user_id).await?;
			info!(%user_id, "Kept admin rights from the admin room");
		}

		let state_lock = services.state.mutex.lock(&room_id).await;
		if let Err(e) = services
			.delete
			.delete_room(&room_id, true, state_lock)
			.await
		{
			warn!(%room_id, "Deleting the admin room: {e}");
		}

		info!(%room_id, "Deleted the admin room");
	}

	// Nobody kept admin rights (say the only admin had left the room): the
	// server's one real account gets them, as the first account would.
	if !services.admin.any_admin().await {
		let people: Vec<_> = services
			.users
			.list_local_users()
			.map(ToOwned::to_owned)
			.collect()
			.await;

		let people: Vec<_> = people
			.into_iter()
			.filter(|user_id| user_id != server_user)
			.collect();

		match people.as_slice() {
			| [only] => {
				services.admin.make_user_admin(only).await?;
				info!(user_id = %only, "Made the server's only account its admin");
			},
			| _ => warn!(
				"No admins left; name one with: anoncat --execute 'users make-user-admin @name:server'"
			),
		}
	}

	if services.users.is_active_local(server_user).await {
		services.users.deactivate_account(server_user).await?;
		services
			.profile
			.set_displayname(server_user, None, None)
			.await
			.ok();
		info!(%server_user, "Deactivated the server bot");
	}

	Ok(())
}
