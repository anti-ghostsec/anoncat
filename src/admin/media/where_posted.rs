use futures::StreamExt;
use ruma::{Mxc, OwnedMxcUri};
use tuwunel_core::{Result, utils::stream::TryIgnore};
use tuwunel_matrix::Event;

use crate::admin_command;

/// Events read back per room looking for the file.
const EVENTS_READ: usize = 50_000;
/// Messages listed per room.
const FOUND_MAX: usize = 10;

/// Who uploaded a file and which messages carry it, with their senders. Only
/// rooms that aren't encrypted can be searched: an encrypted message's file
/// can't be read by the server.
#[admin_command]
pub(super) async fn where_posted(&self, mxc: OwnedMxcUri) -> Result {
	let link = mxc.as_str();
	let parts: Mxc<'_> = link.try_into()?;
	let services = self.services;

	let mut out = if services.globals.server_is_ours(parts.server_name) {
		match services
			.media
			.media_entry(&parts)
			.await
			.and_then(|e| e.user_id)
		{
			| Some(user) => format!("Uploaded here by {user}.\n"),
			| None => "Uploaded here; the uploader isn't recorded.\n".to_owned(),
		}
	} else {
		format!(
			"Uploaded on {}: who uploaded it there isn't known here. Whoever posted it is \
			 below, if it was posted where the server can read it.\n",
			parts.server_name
		)
	};

	let rooms = services.moderation.rooms_of(link).await;
	if rooms.is_empty() {
		out.push_str(
			"Not seen in any room that isn't encrypted. If it was posted in an encrypted room, \
			 the server can't tell which message carries it: ask someone in that room to report \
			 the message from their app, which names the message and its sender.\n",
		);
	}

	for room in rooms {
		let name = services.state_accessor.get_name(&room).await.ok();
		let found: Vec<_> = services
			.timeline
			.pdus_rev(None, &room, None)
			.ignore_err()
			.take(EVENTS_READ)
			.filter(|(_, pdu)| std::future::ready(pdu.content().get().contains(link)))
			.take(FOUND_MAX)
			.collect()
			.await;

		out.push_str(&format!(
			"\nIn {room}{}:\n",
			name.map(|n| format!(" \"{n}\"")).unwrap_or_default()
		));
		if found.is_empty() {
			out.push_str("  no message found in its recent history\n");
		}
		for (_, pdu) in found {
			out.push_str(&format!(
				"  - {} sent {} at {}{}\n",
				pdu.sender(),
				pdu.event_id(),
				pdu.origin_server_ts()
					.to_system_time()
					.map(|t| tuwunel_core::utils::time::format(t, "%Y-%m-%d %H:%M UTC"))
					.unwrap_or_default(),
				if pdu.is_redacted() { " (redacted)" } else { "" },
			));
		}
	}

	self.write_str(&out).await
}
