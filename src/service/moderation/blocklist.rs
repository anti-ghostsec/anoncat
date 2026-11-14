//! Users and whole servers the admins have blocked.
//!
//! A blocked user's messages are hidden from everyone here, past and future,
//! as if every local user ignored them; they can't join rooms through this
//! server or invite anyone on it. A local one's account is also locked, so
//! they can't post at all. A blocked server is all of that for each of its
//! users, and on top it can't make any federation request here, nor gets
//! anything sent to it.
//!
//! Kept in the settings map under `blocked:` and in memory for the checks,
//! which run on every federation request and every event served.

use std::collections::BTreeMap;

use futures::StreamExt;
use ruma::{OwnedServerName, OwnedUserId, ServerName, UserId};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result, implement,
	utils::{
		stream::{ReadyExt, TryIgnore},
		time::now_millis,
	},
	warn,
};

const PREFIX: &[u8] = b"blocked:";

/// Longest reason kept.
const REASON_CHARS: usize = 500;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Blocked {
	/// A user ID or a server name.
	pub target: String,
	/// "user" or "server".
	pub kind: String,
	pub reason: String,
	pub ts: u64,
	pub by: String,
	/// Whether blocking locked a local account, so unblocking unlocks it.
	#[serde(default)]
	pub locked: bool,
}

pub(super) type Blocklist = BTreeMap<String, Blocked>;

/// What a target names, once checked.
enum Target {
	User(OwnedUserId),
	Server(OwnedServerName),
}

/// Reads the stored list; runs once at startup, before anything is checked.
#[implement(super::Service)]
pub(super) async fn load_blocklist(&self) {
	let list: Blocklist = self
		.settings_map
		.raw_stream_prefix(PREFIX)
		.ignore_err()
		.ready_filter_map(|(_, value)| {
			serde_json::from_slice::<Blocked>(value)
				.inspect_err(|e| warn!("Unreadable blocklist entry: {e}"))
				.ok()
		})
		.map(|blocked| (blocked.target.clone(), blocked))
		.collect()
		.await;

	*self.blocked.write().expect("locked") = list;
}

/// Whether a user is blocked, by name or through their server.
#[implement(super::Service)]
#[must_use]
pub fn user_blocked(&self, user: &UserId) -> bool {
	let list = self.blocked.read().expect("locked");
	list.contains_key(user.as_str()) || list.contains_key(user.server_name().as_str())
}

#[implement(super::Service)]
#[must_use]
pub fn server_blocked(&self, server: &ServerName) -> bool {
	self.blocked
		.read()
		.expect("locked")
		.contains_key(server.as_str())
}

/// Whether media from this server is neither fetched nor shown here, by
/// the settings' softer, media-only block.
#[implement(super::Service)]
#[must_use]
pub fn media_server_blocked(&self, server: &ServerName) -> bool {
	self.settings
		.read()
		.expect("locked")
		.media_blocked_servers
		.iter()
		.any(|blocked| blocked.eq_ignore_ascii_case(server.as_str()))
}

/// Everything blocked, newest first.
#[implement(super::Service)]
#[must_use]
pub fn blocklist(&self) -> Vec<Blocked> {
	let mut list: Vec<_> = self
		.blocked
		.read()
		.expect("locked")
		.values()
		.cloned()
		.collect();

	list.sort_by(|a, b| b.ts.cmp(&a.ts));
	list
}

/// Blocks a user (`@name:server`) or a server. A local user's account is
/// locked too when `lock` is set. Their open reports are closed: there is
/// nothing left to decide about them.
#[implement(super::Service)]
pub async fn block(&self, target: &str, reason: &str, by: &UserId, lock: bool) -> Result<Blocked> {
	let target = self.parse_target(target, by)?;
	let (name, kind) = match &target {
		| Target::User(user) => (user.to_string(), "user"),
		| Target::Server(server) => (server.to_string(), "server"),
	};

	let mut locked = false;
	if let Target::User(user) = &target
		&& lock
		&& self.services.globals.user_is_local(user)
		&& !self.services.users.is_locked(user).await
	{
		self.services.users.set_locked(user, by);
		locked = true;
	}

	let blocked = Blocked {
		target: name.clone(),
		kind: kind.into(),
		reason: reason.trim().chars().take(REASON_CHARS).collect(),
		ts: now_millis(),
		by: by.to_string(),
		locked,
	};

	self.settings_map
		.insert(&key(&name), serde_json::to_vec(&blocked)?);
	self.blocked
		.write()
		.expect("locked")
		.insert(name.clone(), blocked.clone());

	let closed = self.close_reports_from(&target).await;
	let reason = if blocked.reason.is_empty() { "no reason given" } else { &blocked.reason };
	self.services
		.admin
		.notify(&format!(
			"{by} blocked {kind} {name} ({reason}).{}{}",
			if locked { " Their account is locked." } else { "" },
			if closed > 0 { format!(" {closed} open reports about them closed.") } else { String::new() },
		))
		.await;

	Ok(blocked)
}

/// Lifts a block, and the lock it put on a local account.
#[implement(super::Service)]
pub async fn unblock(&self, target: &str, by: &UserId) -> Result<Blocked> {
	let Some(blocked) = self
		.blocked
		.write()
		.expect("locked")
		.remove(target)
	else {
		return Err!(Request(NotFound("{target} isn't blocked.")));
	};

	self.settings_map.remove(&key(target));

	if blocked.locked
		&& let Ok(user) = UserId::parse(target)
	{
		self.services.users.clear_locked(&user);
	}

	self.services
		.admin
		.notify(&format!("{by} unblocked {} {target}.", blocked.kind))
		.await;

	Ok(blocked)
}

#[implement(super::Service)]
fn parse_target(&self, target: &str, by: &UserId) -> Result<Target> {
	let target = target.trim();
	let ours = self.services.globals.server_name();

	if target.starts_with('@') {
		let Ok(user) = UserId::parse(target) else {
			return Err!(Request(InvalidParam("\"{target}\" isn't a user ID like @name:server.")));
		};
		if user == by || user == self.services.globals.server_user {
			return Err!(Request(InvalidParam("That account can't be blocked.")));
		}
		return Ok(Target::User(user));
	}

	let Ok(server) = ServerName::parse(target) else {
		return Err!(Request(InvalidParam(
			"\"{target}\" isn't a user ID (@name:server) or a server name."
		)));
	};
	if server == ours {
		return Err!(Request(InvalidParam("This server can't block itself.")));
	}

	Ok(Target::Server(server))
}

/// Marks open reports about the target's messages resolved; returns how many.
#[implement(super::Service)]
async fn close_reports_from(&self, target: &Target) -> usize {
	let from_target = |sender: &str| match target {
		| Target::User(user) => sender == user.as_str(),
		| Target::Server(server) => UserId::parse(sender).is_ok_and(|s| s.server_name() == server),
	};

	let mut closed = 0;
	for mut report in self.reports(false, usize::MAX).await {
		if report.kind != "room" && from_target(&report.sender) {
			report.status = "resolved".into();
			self.save_report(&report);
			closed += 1;
		}
	}

	closed
}

fn key(target: &str) -> Vec<u8> { [PREFIX, target.as_bytes()].concat() }
