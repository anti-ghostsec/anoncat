//! An admin's own picture to show in place of hidden and blocked media,
//! instead of one of the built-in ones. Kept in the settings map, without
//! its metadata.

use std::sync::Arc;

use tuwunel_core::{Err, Result, implement};

use crate::media::strip::strip_metadata;

const KEY: &[u8] = b"stand_in_image";

/// Largest picture taken; it's sent every time something is replaced.
const MAX_BYTES: usize = 2 << 20;

/// The built-in pictures, by the name the settings keep.
pub const BUILT_IN: [&str; 4] = ["orange", "grey", "black", "plain"];

#[derive(Debug)]
pub struct StandIn {
	pub content: Vec<u8>,
	pub content_type: &'static str,
}

#[implement(super::Service)]
pub(super) async fn load_stand_in(&self) {
	if let Ok(stored) = self.settings_map.get(KEY).await
		&& let Some(content_type) = image_type(&stored)
	{
		*self.custom_stand_in.write().expect("locked") =
			Some(Arc::new(StandIn { content: stored.to_vec(), content_type }));
	}
}

/// The uploaded picture, if there is one.
#[implement(super::Service)]
#[must_use]
pub fn custom_stand_in(&self) -> Option<Arc<StandIn>> {
	self.custom_stand_in.read().expect("locked").clone()
}

/// Takes a PNG, JPEG or WebP, drops its metadata and keeps it.
#[implement(super::Service)]
pub fn set_custom_stand_in(&self, file: &[u8]) -> Result<Arc<StandIn>> {
	if file.len() > MAX_BYTES {
		return Err!(Request(TooLarge("Pictures up to 2 MiB only.")));
	}
	let Some(content_type) = image_type(file) else {
		return Err!(Request(InvalidParam("Only PNG, JPEG or WebP pictures.")));
	};

	let content = strip_metadata(file).unwrap_or_else(|| file.to_vec());
	self.settings_map.insert(KEY, &content);
	let stand_in = Arc::new(StandIn { content, content_type });
	*self.custom_stand_in.write().expect("locked") = Some(stand_in.clone());

	Ok(stand_in)
}

#[implement(super::Service)]
pub fn clear_custom_stand_in(&self) {
	self.settings_map.remove(KEY);
	*self.custom_stand_in.write().expect("locked") = None;
}

/// The type a file's first bytes show; only kinds the stripper handles.
fn image_type(file: &[u8]) -> Option<&'static str> {
	match file {
		| [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
		| [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
		| [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
		| _ => None,
	}
}
