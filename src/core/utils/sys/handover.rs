//! Listening sockets carried across an in-place exec restart.
//!
//! The service manager passes its sockets once, at fds 3 and up. The server
//! keeps a duplicate of each so that closing its listeners during shutdown
//! never closes the socket itself: connections arriving while the next image
//! starts wait in the kernel's backlog instead of being refused. Just before
//! the exec the duplicates are moved back to fds 3 and up and the
//! `LISTEN_*` variables are rebuilt for the next image, which keeps this PID.

use std::{
	os::fd::RawFd,
	sync::{Mutex, PoisonError},
};

use crate::Result;

const SD_LISTEN_FDS_START: RawFd = 3;

/// Above anything the exec targets, so moving one never overwrites another.
const PARK_FLOOR: RawFd = 1024;

static KEPT: Mutex<Vec<(RawFd, String)>> = Mutex::new(Vec::new());

/// Keeps a close-on-exec duplicate of a passed listener under its name.
pub fn keep(fd: RawFd, name: &str) -> Result {
	// SAFETY: F_DUPFD_CLOEXEC creates a new descriptor and touches nothing else.
	let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, PARK_FLOOR) };
	if dup < 0 {
		return Err(std::io::Error::last_os_error().into());
	}

	KEPT.lock()
		.unwrap_or_else(PoisonError::into_inner)
		.push((dup, name.to_owned()));

	Ok(())
}

/// Places the kept sockets at fds 3 and up, inheritable, and returns the
/// `LISTEN_*` variables describing them; empty when nothing was kept. Only
/// for the moment before exec: whatever held those fd numbers is replaced.
#[must_use]
pub fn prepare_exec() -> Vec<(&'static str, String)> {
	let kept = std::mem::take(&mut *KEPT.lock().unwrap_or_else(PoisonError::into_inner));
	if kept.is_empty() {
		return Vec::new();
	}

	let mut names = Vec::with_capacity(kept.len());
	for (i, (fd, name)) in kept.iter().enumerate() {
		let target = SD_LISTEN_FDS_START.saturating_add(i.try_into().unwrap_or(RawFd::MAX));

		// SAFETY: dup2 replaces `target` with the kept socket and clears its
		// close-on-exec flag. Every kept fd sits at PARK_FLOOR or above, so no
		// target is one of them.
		if unsafe { libc::dup2(*fd, target) } < 0 {
			return Vec::new();
		}

		names.push(name.as_str());
	}

	vec![
		("LISTEN_PID", std::process::id().to_string()),
		("LISTEN_FDS", kept.len().to_string()),
		("LISTEN_FDNAMES", names.join(":")),
	]
}
