use ruma::OwnedMxcUri;
use tuwunel_core::Result;

use crate::admin_command;

/// Swaps a file for the stand-in for good, everywhere it's posted.
#[admin_command]
pub(super) async fn swap(&self, mxc: OwnedMxcUri, reason: Vec<String>) -> Result {
	let (thumbs, closed) = self
		.services
		.media
		.swap_with_thumbnails(mxc.as_str(), &reason.join(" "))
		.await?;

	let reports = match closed {
		| 0 => String::new(),
		| 1 => " Its open report is closed.".to_owned(),
		| n => format!(" Its {n} open reports are closed."),
	};
	let thumbs = match thumbs {
		| 0 => String::new(),
		| 1 => " The thumbnail posted with it is swapped too.".to_owned(),
		| n => format!(" The {n} thumbnails posted with it are swapped too."),
	};
	self.write_str(&format!(
		"Swapped for the stand-in: the file is deleted, and everyone now sees the stand-in in its place. This can't be undone.{thumbs}{reports}"
	))
	.await
}

/// Hides a file until review: it's kept, but served to nobody.
#[admin_command]
pub(super) async fn hide(&self, mxc: OwnedMxcUri, reason: Vec<String>) -> Result {
	self.services
		.media
		.hide(mxc.as_str(), &reason.join(" "))?;

	self.write_str("Hidden: nobody is served it until it's shown again with media unhide.")
		.await
}

/// Serves a hidden file again.
#[admin_command]
pub(super) async fn unhide(&self, mxc: OwnedMxcUri) -> Result {
	self.services.media.unhide(mxc.as_str())?;
	self.write_str("Shown again, unless it was swapped.").await
}
