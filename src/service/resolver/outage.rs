//! Notices when our own DNS stops answering and, once it answers again, lifts
//! the retry holds that the outage put on other servers.
//!
//! A lookup that times out is our fault, not the peer's: the network (here,
//! the VPN tunnel) is down. Every server tried meanwhile still collects a
//! failure and a growing retry delay, so after the network returns most of
//! federation would sit idle for hours. When answers come back, servers whose
//! newest failure falls inside the outage are marked alive and retried now.

use std::sync::{Arc, Mutex};

use tuwunel_core::{implement, info, utils::time::now_secs};

/// Failures this long before the first timed-out lookup still count as the
/// outage: connections fail before DNS does, and a lookup takes a while to
/// time out.
const LEAD_SECS: u64 = 60;

/// Answers only end an outage once no lookup has timed out for this long, so
/// one answer from the cache while the network is still down doesn't.
const QUIET_SECS: u64 = 30;

#[derive(Default)]
pub(super) struct Outage(Mutex<Option<Window>>);

#[derive(Clone, Copy)]
struct Window {
	since: u64,
	last_timeout: u64,
}

impl Outage {
	/// A lookup got no answer at all (timed out or found no connection).
	fn unanswered(&self, now: u64) {
		let mut window = self.0.lock().expect("locked");
		let window = window.get_or_insert(Window { since: now, last_timeout: now });
		window.last_timeout = now;
	}

	/// A lookup was answered; returns when the outage began if this ends one.
	fn answered(&self, now: u64) -> Option<u64> {
		let mut window = self.0.lock().expect("locked");
		let quiet = window.is_some_and(|w| now.saturating_sub(w.last_timeout) >= QUIET_SECS);

		quiet
			.then(|| window.take())
			.flatten()
			.map(|w| w.since.saturating_sub(LEAD_SECS))
	}
}

#[implement(super::Service)]
pub(super) fn dns_unanswered(&self) { self.outage.unanswered(now_secs()); }

#[implement(super::Service)]
pub(super) fn dns_answered(&self) {
	let now = now_secs();
	let Some(since) = self.outage.answered(now) else {
		return;
	};

	let services = Arc::clone(&self.services);
	self.services.server.runtime().spawn(async move {
		let held: Vec<_> = services
			.federation
			.peer_backoffs()
			.await
			.into_iter()
			.filter(|(_, backoff)| backoff.anchor_secs >= since)
			.map(|(server, _)| server)
			.collect();

		for server in &held {
			services.sending.notify_peer_alive(server).await;
		}

		info!(
			down_secs = now.saturating_sub(since),
			retrying = held.len(),
			"DNS answers again after an outage; retrying servers that failed meanwhile"
		);
	});
}

#[cfg(test)]
mod tests {
	use super::{LEAD_SECS, Outage, QUIET_SECS};

	#[test]
	fn answers_end_an_outage_only_after_a_quiet_spell() {
		let outage = Outage::default();
		assert_eq!(outage.answered(1_000), None, "no outage, nothing to end");

		outage.unanswered(1_000);
		outage.unanswered(1_100);
		assert_eq!(outage.answered(1_105), None, "a cached answer mid-outage");
		assert_eq!(outage.answered(1_100 + QUIET_SECS), Some(1_000 - LEAD_SECS));
		assert_eq!(outage.answered(2_000), None, "already ended");
	}
}
