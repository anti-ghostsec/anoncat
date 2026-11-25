//! Twin accounts on a second server.
//!
//! Two servers on one machine form a pair. The *source* (the one with
//! `twin_server_name` and `twin_api` set) keeps a twin of each of its
//! accounts on the other, the *twin side* (the one with `twin_of` set): same
//! name, same password hash, deactivated with it. Twins are invited to the
//! person's private rooms and the rooms they run, with the same power level,
//! and join them. Nothing here names either server: swapping the settings
//! swaps the roles.
//!
//! The source talks to the twin side over loopback HTTP on the twin's admin
//! panel listener, with a shared secret (`twin_secret_file`):
//!
//! - `PUT /_anoncat/twin/v1/users/{localpart}` makes or updates a twin;
//! - `POST /_anoncat/twin/v1/users/{localpart}/join` joins it to a room it
//!   was invited to.
//! - `POST /_anoncat/twin/v1/panel-token` gives the source's admin panel a
//!   short-lived token of the twin side's server account, so one panel runs
//!   both servers.
//!
//! Leaving a room doesn't remove the twin from it, and direct chats are left
//! alone: the person's twin can be invited there like anyone else.

mod panel;
mod source;
mod target;

pub use self::{panel::Answer, target::PANEL_TOKEN_LIFETIME};

use std::{
	collections::HashMap,
	path::Path,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tuwunel_core::{Err, Result, utils::hash::sha256, warn};

/// Shortest secret accepted; the file is meant to hold 64 hex characters.
const MIN_SECRET_LEN: usize = 32;

/// Warnings about an unreachable twin side, at most one this often.
const QUIET_WARNING_INTERVAL: Duration = Duration::from_hours(1);

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	/// Source side: what each twin was last sent, by localpart.
	pushed: Mutex<HashMap<String, sha256::Digest>>,
	/// Source side: the localpart the room sync goes on from.
	cursor: Mutex<Option<String>>,
	last_quiet_warning: Mutex<Option<Instant>>,
	/// Source side: the twin side's panel token and when it runs out.
	panel_token: tokio::sync::Mutex<Option<(String, Instant)>>,
	panel_client: std::sync::OnceLock<reqwest::Client>,
}

/// A twin as the source sends it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
	/// Argon2 PHC string; none for an account without a password.
	#[serde(default)]
	pub password_hash: Option<String>,
	#[serde(default)]
	pub deactivated: bool,
	/// Locked: can't sign in or use the account (MSC3939).
	#[serde(default)]
	pub locked: bool,
	/// Suspended: can read but not post (MSC3823).
	#[serde(default)]
	pub suspended: bool,
	#[serde(default)]
	pub displayname: Option<String>,
	/// Markdown sent to the twin when it is made: the source's welcome
	/// message, for accounts new there.
	#[serde(default)]
	pub welcome: Option<String>,
}

impl Account {
	/// What decides whether the twin needs updating; the welcome text isn't
	/// part of the account.
	fn digest(&self) -> sha256::Digest {
		let state = (
			&self.password_hash,
			self.deactivated,
			&self.displayname,
			self.locked,
			self.suspended,
		);
		sha256::hash(serde_json::to_vec(&state).unwrap_or_default())
	}
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct AccountResult {
	pub created: bool,
	pub updated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
	pub room_id: ruma::OwnedRoomId,
	#[serde(default)]
	pub via: Vec<ruma::OwnedServerName>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			pushed: Mutex::default(),
			cursor: Mutex::default(),
			last_quiet_warning: Mutex::default(),
			panel_token: tokio::sync::Mutex::default(),
			panel_client: std::sync::OnceLock::new(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result { self.source_worker().await }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Whether this server keeps twins of its accounts on another one.
	#[must_use]
	pub fn is_source(&self) -> bool {
		let config = &self.services.server.config;
		config.twin_server_name.is_some()
			&& config.twin_api.is_some()
			&& config.twin_secret_file.is_some()
	}

	/// Whether this server holds the twins of another one's accounts.
	#[must_use]
	pub fn is_twin(&self) -> bool {
		let config = &self.services.server.config;
		config.twin_of.is_some() && config.twin_secret_file.is_some()
	}

	/// Source side: whether this account's twin was updated since startup.
	#[must_use]
	pub fn has_pushed(&self, localpart: &str) -> bool {
		self.pushed
			.lock()
			.expect("locked")
			.contains_key(localpart)
	}

	/// Twin side: whether a request carries the shared secret. Compared in
	/// constant time; a missing or unreadable secret file lets nobody in.
	pub async fn secret_matches(&self, given: &str) -> bool {
		let Some(path) = &self.services.server.config.twin_secret_file else {
			return false;
		};

		match read_secret(path).await {
			| Ok(secret) => secret_eq(&secret, given),
			| Err(e) => {
				warn!("Twin secret unreadable: {e}");
				false
			},
		}
	}

	/// A warning that repeats every run otherwise (the twin side down): once
	/// an hour.
	fn warn_quietly(&self, message: &str) {
		let mut last = self.last_quiet_warning.lock().expect("locked");
		if last.is_none_or(|last| last.elapsed() >= QUIET_WARNING_INTERVAL) {
			*last = Some(Instant::now());
			warn!("{message}");
		}
	}
}

/// The secret in the file, without surrounding whitespace.
async fn read_secret(path: &Path) -> Result<String> {
	let secret = tokio::fs::read_to_string(path).await?;
	let secret = secret.trim();
	if secret.len() < MIN_SECRET_LEN || !secret.bytes().all(|b| b.is_ascii_graphic()) {
		return Err!("the file must hold at least {MIN_SECRET_LEN} characters, without spaces");
	}

	Ok(secret.to_owned())
}

/// Constant-time comparison; an empty secret never matches.
fn secret_eq(secret: &str, given: &str) -> bool {
	!secret.is_empty() && bool::from(secret.as_bytes().ct_eq(given.as_bytes()))
}

#[cfg(test)]
mod tests {
	use super::{Account, secret_eq};

	#[test]
	fn secrets_compare_whole() {
		let secret = "0123456789abcdef0123456789abcdef";

		assert!(secret_eq(secret, secret));
		assert!(!secret_eq(secret, ""));
		assert!(!secret_eq(secret, "0123456789abcdef"));
		assert!(!secret_eq(secret, &format!("{secret}0")));
		assert!(!secret_eq(secret, "0123456789abcdef0123456789abcdeF"));
		assert!(!secret_eq("", ""));
	}

	#[test]
	fn digest_follows_the_account_not_the_welcome() {
		let account = Account {
			password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into()),
			displayname: Some("Alice".into()),
			..Account::default()
		};

		let welcomed = Account { welcome: Some("Hi".into()), ..account.clone() };
		assert_eq!(account.digest(), welcomed.digest());

		let renamed = Account { displayname: Some("Al".into()), ..account.clone() };
		assert_ne!(account.digest(), renamed.digest());

		let new_password = Account { password_hash: Some("other".into()), ..account.clone() };
		assert_ne!(account.digest(), new_password.digest());

		let deactivated = Account { deactivated: true, ..account.clone() };
		assert_ne!(account.digest(), deactivated.digest());
	}
}
