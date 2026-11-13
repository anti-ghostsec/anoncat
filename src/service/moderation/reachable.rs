//! Whether people outside can reach the server: every few minutes it asks
//! its own public address for its federation version, the way another server
//! would. That goes out through the VPN, to the relay, and back down the
//! relay's tunnel, so a dead tunnel, a stopped relay, a kill-switch rule or
//! wrong DNS all show up as a failure. Two failures in a row make a loud
//! notice; coming back makes another.

use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

use serde::Serialize;
use tuwunel_core::{implement, utils::time::now_millis};

use super::Service;

/// Between checks.
const EVERY: Duration = Duration::from_secs(5 * 60);
/// First check after start, once everything is up.
const FIRST: Duration = Duration::from_secs(90);
/// How long one check may take.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Failures in a row before the admin is told.
const TELL_AFTER: u32 = 2;

/// The latest result, for the dashboard.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Reachability {
	/// None until the first check.
	pub ok: Option<bool>,
	pub checked_ts: u64,
	/// When the current state began.
	pub since_ts: u64,
	pub error: Option<String>,
	pub failures: u32,
}

pub(super) type State = Mutex<Reachability>;

#[implement(Service)]
#[must_use]
pub fn reachability(&self) -> Reachability { self.reachable.lock().expect("locked").clone() }

/// Runs the checks until shutdown.
#[implement(Service)]
pub(super) async fn watch_reachability(self: Arc<Self>) {
	let server = &self.services.server;
	tokio::select! {
		() = tokio::time::sleep(FIRST) => {},
		() = server.until_shutdown() => return,
	}

	loop {
		let result = self.check_reachable().await;
		self.record_reachable(result).await;

		tokio::select! {
			() = tokio::time::sleep(EVERY) => {},
			() = server.until_shutdown() => return,
		}
	}
}

#[implement(Service)]
async fn check_reachable(&self) -> Result<(), String> {
	let name = self.services.globals.server_name();
	let url = format!("https://{name}/_matrix/federation/v1/version");
	let response = self
		.client()
		.get(&url)
		.timeout(TIMEOUT)
		.send()
		.await
		.map_err(|e| {
			if e.is_timeout() {
				"no answer within 20 seconds".to_owned()
			} else if e.is_connect() {
				"couldn't connect".to_owned()
			} else {
				"the request failed".to_owned()
			}
		})?;

	if !response.status().is_success() {
		return Err(format!("answered {}", response.status()));
	}

	Ok(())
}

#[implement(Service)]
async fn record_reachable(&self, result: Result<(), String>) {
	let now = now_millis();
	let notice = {
		let mut state = self.reachable.lock().expect("locked");
		state.checked_ts = now;
		match result {
			| Ok(()) => {
				let was_down = state.failures >= TELL_AFTER;
				let down_since = state.since_ts;
				if state.ok != Some(true) {
					state.since_ts = now;
				}
				state.ok = Some(true);
				state.error = None;
				state.failures = 0;
				was_down.then(|| {
					let minutes = now.saturating_sub(down_since) / 60_000;
					format!("anoncat can be reached from outside again, after about {minutes} minutes.")
				})
			},
			| Err(error) => {
				if state.ok != Some(false) {
					state.since_ts = now;
				}
				state.ok = Some(false);
				state.failures = state.failures.saturating_add(1);
				state.error = Some(error.clone());
				(state.failures == TELL_AFTER).then(|| {
					format!(
						"anoncat can't be reached from outside: {error}. Apps and other servers \
						 can't get in. Check that the relay tunnel is up (anoncat-relay), the VPN \
						 is connected, and the relay VPS is running."
					)
				})
			},
		}
	};

	if let Some(notice) = notice {
		self.services.admin.notify_loud(&notice).await;
	}
}
