//! The welcome message: a notice every account made after it was set gets
//! once, by itself.
//!
//! The text is set from the panel. Setting it marks every account that
//! exists then as already welcomed (they are reached with an announcement
//! instead), so only accounts made afterwards get it. Clearing it stops it
//! for new accounts; setting it again marks everyone again. A twin made on
//! the twin server for a new account brings the text along (see `twins`).

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use futures::StreamExt;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{Err, Result, implement, info, utils::stream::ReadyExt, warn};

use super::Service;

/// Keys in `anoncat_settings`.
const WELCOME_KEY: &str = "welcome_notice";
const WELCOMED_KEY: &str = "welcomed_users";
/// Accounts welcomed here whose twin hasn't been sent the text yet.
const TWIN_PENDING_KEY: &str = "welcome_twin_pending";

/// How often new accounts are looked for.
const INTERVAL: Duration = Duration::from_mins(1);

/// Longest welcome message, in characters.
pub const MAX_WELCOME: usize = 20_000;

#[implement(Service)]
pub(super) async fn welcome_worker(self: Arc<Self>) -> Result {
	self.name_server_account().await;

	loop {
		tokio::select! {
			() = tokio::time::sleep(INTERVAL) => {},
			() = self.services.server.until_shutdown() => return Ok(()),
		}

		match self.deliver_welcome().await {
			| Ok(0) => {},
			| Ok(sent) => info!(sent, "Welcome message sent to new accounts"),
			| Err(e) => warn!("Welcome message run failed: {e}"),
		}
	}
}

/// The welcome message, if one is set.
#[implement(Service)]
pub async fn welcome(&self) -> Option<String> {
	self.settings
		.get(WELCOME_KEY)
		.await
		.ok()
		.and_then(|stored| serde_json::from_slice::<String>(&stored).ok())
		.filter(|text| !text.is_empty())
}

/// Sets the welcome message; `None` or blank clears it. The first time it is
/// set (after none), every account here is marked as already welcomed.
#[implement(Service)]
pub async fn set_welcome(&self, text: Option<&str>) -> Result {
	let text = text.map(str::trim).filter(|text| !text.is_empty());
	if text.is_some_and(|text| text.chars().count() > MAX_WELCOME) {
		return Err!(Request(InvalidParam(
			"The message is longer than {MAX_WELCOME} characters."
		)));
	}

	let _lock = self.welcome_lock.lock().await;
	let previous = self.welcome().await;

	if is_first_set(previous.as_deref(), text) {
		let existing: Vec<String> = self
			.services
			.users
			.stream()
			.ready_filter(|user| self.services.globals.user_is_local(user))
			.map(|user| user.localpart().to_owned())
			.collect()
			.await;

		let mut welcomed = self.read_set(WELCOMED_KEY).await;
		mark_existing(&mut welcomed, existing);
		self.write_set(WELCOMED_KEY, &welcomed)?;
	}

	match text {
		| Some(text) => self
			.settings
			.insert(WELCOME_KEY, serde_json::to_vec(text)?),
		| None => self.settings.remove(WELCOME_KEY),
	}

	info!(set = text.is_some(), "Welcome message changed from the panel");

	Ok(())
}

/// The welcome text a newly made twin of this account should get: while the
/// account here hasn't had it yet, or got it before its twin did.
#[implement(Service)]
pub async fn welcome_for_twin(&self, localpart: &str) -> Option<String> {
	let text = self.welcome().await?;
	let welcomed = self.read_set(WELCOMED_KEY).await;
	let pending = self.read_set(TWIN_PENDING_KEY).await;

	(!welcomed.contains(localpart) || pending.contains(localpart)).then_some(text)
}

/// The twin of this account has been told; it no longer needs the text.
#[implement(Service)]
pub async fn twin_welcome_sent(&self, localpart: &str) -> Result {
	let _lock = self.welcome_lock.lock().await;
	let mut pending = self.read_set(TWIN_PENDING_KEY).await;
	if pending.remove(localpart) {
		self.write_set(TWIN_PENDING_KEY, &pending)?;
	}

	Ok(())
}

/// Marks an account as welcomed without sending anything: a twin made for
/// another server's account is welcomed by that server.
#[implement(Service)]
pub async fn mark_welcomed(&self, localpart: &str) -> Result {
	let _lock = self.welcome_lock.lock().await;
	let mut welcomed = self.read_set(WELCOMED_KEY).await;
	if welcomed.insert(localpart.to_owned()) {
		self.write_set(WELCOMED_KEY, &welcomed)?;
	}

	Ok(())
}

/// Sends the welcome message to every account that can sign in and hasn't
/// had it. Returns how many got it.
#[implement(Service)]
async fn deliver_welcome(&self) -> Result<usize> {
	let Some(text) = self.welcome().await else {
		return Ok(0);
	};

	let server_user: &UserId = &self.services.globals.server_user;
	let welcomed = self.read_set(WELCOMED_KEY).await;
	let new: Vec<OwnedUserId> = self
		.services
		.users
		.list_local_users()
		.ready_filter(|user| *user != server_user && !welcomed.contains(user.localpart()))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let twins = self.services.twins.is_source();
	let mut sent = 0_usize;
	let mut failed = 0_usize;
	for user in &new {
		if let Err(e) = self.send(user, &text).await {
			failed = failed.saturating_add(1);
			tuwunel_core::debug_warn!("Welcome message not delivered: {e}");
			continue;
		}

		sent = sent.saturating_add(1);
		let localpart = user.localpart();
		let _lock = self.welcome_lock.lock().await;
		let mut welcomed = self.read_set(WELCOMED_KEY).await;
		welcomed.insert(localpart.to_owned());
		self.write_set(WELCOMED_KEY, &welcomed)?;

		if twins && !self.services.twins.has_pushed(localpart) {
			let mut pending = self.read_set(TWIN_PENDING_KEY).await;
			pending.insert(localpart.to_owned());
			self.write_set(TWIN_PENDING_KEY, &pending)?;
		}
	}

	if failed > 0 {
		warn!(failed, "Welcome message not delivered to some accounts; trying again later");
	}

	Ok(sent)
}

#[implement(Service)]
async fn read_set(&self, key: &str) -> BTreeSet<String> {
	self.settings
		.get(key)
		.await
		.ok()
		.and_then(|stored| serde_json::from_slice(&stored).ok())
		.unwrap_or_default()
}

#[implement(Service)]
fn write_set(&self, key: &str, set: &BTreeSet<String>) -> Result {
	self.settings.insert(key, serde_json::to_vec(set)?);

	Ok(())
}

/// Whether a change sets the message where there was none, which marks the
/// accounts there are then as welcomed. Changing the text of a message
/// already set doesn't.
fn is_first_set(previous: Option<&str>, next: Option<&str>) -> bool {
	previous.is_none() && next.is_some()
}

/// Adds the accounts there are now to the welcomed ones.
fn mark_existing(welcomed: &mut BTreeSet<String>, existing: impl IntoIterator<Item = String>) {
	welcomed.extend(existing);
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::{is_first_set, mark_existing};

	#[test]
	fn only_setting_it_after_none_marks_everyone() {
		assert!(is_first_set(None, Some("hi")));
		assert!(!is_first_set(Some("hi"), Some("hello")));
		assert!(!is_first_set(Some("hi"), None));
		assert!(!is_first_set(None, None));
	}

	#[test]
	fn first_set_marks_existing_accounts_only() {
		let mut welcomed: BTreeSet<String> = ["old".to_owned()].into();
		mark_existing(&mut welcomed, ["alice".to_owned(), "bob".to_owned()]);

		assert!(welcomed.contains("alice"));
		assert!(welcomed.contains("bob"));
		assert!(welcomed.contains("old"));
		assert!(!welcomed.contains("carol"), "made later, so still to be welcomed");
	}
}
