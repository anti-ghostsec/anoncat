//! The picture people here see in place of media they shouldn't: media hidden
//! by an admin or the AI, and anything from a server that's blocked or barred
//! from media downloads. Without it they'd see a broken image. A cat by
//! default; the admin picks another built-in one or uploads their own.

use ruma::Mxc;
use tuwunel_core::{
	implement,
	utils::{content_disposition::make_content_disposition, random_string},
};

use super::Media;

/// Drawn for anoncat; they hold no metadata, only the picture.
const ORANGE: &[u8] = include_bytes!("stand_in/orange.png");
const GREY: &[u8] = include_bytes!("stand_in/grey.png");
const BLACK: &[u8] = include_bytes!("stand_in/black.png");
const PLAIN: &[u8] = include_bytes!("stand_in/plain.png");

/// Stand-ins aren't kept by clients, so the real file shows once it's allowed
/// again.
pub const CACHE_CONTROL_NO_STORE: &str = "no-store";

/// The stand-in, when this item should be replaced and stand-ins are on.
#[implement(super::Service)]
#[must_use]
pub fn stand_in(&self, mxc: &Mxc<'_>) -> Option<Media> {
	let choice = self.services.moderation.stand_in_image()?;

	let config = &self.services.server.config;
	let refused = self.is_hidden(mxc)
		|| (!self.services.globals.server_is_ours(mxc.server_name)
			&& (self
				.services
				.moderation
				.server_blocked(mxc.server_name)
				|| config.is_forbidden_remote_server_name(mxc.server_name)
				|| config
					.prevent_media_downloads_from
					.is_match(mxc.server_name.host())));

	refused.then(|| self.stand_in_picture(&choice))
}

/// The named picture; the orange cat when it's unknown or none was uploaded.
#[implement(super::Service)]
#[must_use]
pub fn stand_in_picture(&self, choice: &str) -> Media {
	let (content, content_type) = match choice {
		| "custom" => match self.services.moderation.custom_stand_in() {
			| Some(custom) => (custom.content.clone(), custom.content_type),
			| None => (ORANGE.to_vec(), "image/png"),
		},
		| "grey" => (GREY.to_vec(), "image/png"),
		| "black" => (BLACK.to_vec(), "image/png"),
		| "plain" => (PLAIN.to_vec(), "image/png"),
		| _ => (ORANGE.to_vec(), "image/png"),
	};

	// A fresh name each time, so nothing about it says it's a stand-in.
	let extension = content_type.trim_start_matches("image/");
	let name = format!("{}.{extension}", random_string(12).to_lowercase());

	Media {
		content,
		content_type: Some(content_type.into()),
		content_disposition: Some(make_content_disposition(
			None,
			Some(content_type),
			Some(&name),
		)),
	}
}
