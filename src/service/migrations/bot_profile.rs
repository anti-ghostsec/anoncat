use ruma::events::room::topic::{RoomTopicEventContent, TopicContentBlock};
use tuwunel_core::{Result, info};
use tuwunel_matrix::pdu::PduBuilder;

use crate::{
	Services,
	admin::create::{BOT_NAME, admin_room_topic},
	profile::Propagation,
};

pub(super) const MARKER: &str = "anoncat_bot_profile";

/// Gives an existing server's bot anoncat's name and admin room topic; new
/// servers get both when the admin room is created.
pub(super) async fn bot_profile(services: &Services) -> Result {
	let Ok(room_id) = services.admin.get_admin_room().await else {
		return Ok(());
	};

	let server_user = services.globals.server_user.as_ref();
	services
		.profile
		.set_displayname(server_user, Some(BOT_NAME), Some(Propagation::All))
		.await?;

	let state_lock = services.state.mutex.lock(&room_id).await;
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomTopicEventContent {
				topic_block: TopicContentBlock::default(),
				topic: admin_room_topic(services),
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.await?;

	info!("Renamed the server bot to {BOT_NAME} and updated the admin room topic");
	Ok(())
}
