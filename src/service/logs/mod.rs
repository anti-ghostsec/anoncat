//! The last server log lines, kept in memory for the panel's Logs page.
//!
//! Info, warnings and errors are copied into a fixed-size ring as they are
//! logged; nothing is written anywhere. IP addresses are blanked before a
//! line is stored, as a second guard on top of the privacy pass that keeps
//! them out of log lines in the first place.

use std::{
	collections::VecDeque,
	fmt::Write,
	net::{Ipv4Addr, Ipv6Addr},
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::Serialize;
use tuwunel_core::{
	Result,
	log::{
		Level,
		capture::{Capture, Guard},
	},
	utils::time::now_millis,
};

/// Lines kept.
const KEPT: usize = 2000;

/// Longest line kept, in bytes.
const LINE_LIMIT: usize = 2048;

const REDACTED: &str = "[ip]";

pub struct Service {
	lines: Arc<Mutex<VecDeque<Line>>>,
	_capture: Guard,
}

/// One log line.
#[derive(Clone, Debug, Serialize)]
pub struct Line {
	pub ts: u64,
	/// "ERROR", "WARN" or "INFO".
	pub level: &'static str,
	/// The module that logged it.
	pub target: String,
	pub message: String,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let lines: Arc<Mutex<VecDeque<Line>>> = Arc::default();
		let ring = lines.clone();
		let capture = Capture::new(
			&args.server.log.capture,
			Some(|data: tuwunel_core::log::capture::Data<'_>| data.level() <= Level::INFO),
			move |data: tuwunel_core::log::capture::Data<'_>| {
				let mut message = String::with_capacity(128);
				message.push_str(data.message());
				for (name, value) in data.values {
					// `_debug` marks a debug-level event logged at info.
					if *name != "message" && *name != "_debug" {
						write!(message, " {name}={value}").ok();
					}
				}

				let mut message = scrub_ips(&message);
				truncate(&mut message, LINE_LIMIT);
				let line = Line {
					ts: now_millis(),
					level: level_name(data.level()),
					target: data.mod_name().to_owned(),
					message,
				};

				let mut lines = ring.lock().expect("locked");
				if lines.len() >= KEPT {
					lines.pop_front();
				}
				lines.push_back(line);
			},
		);

		Ok(Arc::new(Self { lines, _capture: capture.start() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Up to `limit` of the newest lines at `level` or worse, oldest first.
	#[must_use]
	pub fn lines(&self, level: Level, limit: usize) -> Vec<Line> {
		let lines = self.lines.lock().expect("locked");
		let mut out: Vec<Line> = lines
			.iter()
			.rev()
			.filter(|line| parse_level(line.level) <= level)
			.take(limit)
			.cloned()
			.collect();

		out.reverse();
		out
	}
}

fn level_name(level: Level) -> &'static str {
	match level {
		| Level::ERROR => "ERROR",
		| Level::WARN => "WARN",
		| Level::INFO => "INFO",
		| Level::DEBUG => "DEBUG",
		| Level::TRACE => "TRACE",
	}
}

fn parse_level(name: &str) -> Level {
	match name {
		| "ERROR" => Level::ERROR,
		| "WARN" => Level::WARN,
		| "INFO" => Level::INFO,
		| "DEBUG" => Level::DEBUG,
		| _ => Level::TRACE,
	}
}

fn truncate(s: &mut String, limit: usize) {
	if s.len() > limit {
		let mut end = limit;
		while !s.is_char_boundary(end) {
			end = end.saturating_sub(1);
		}
		s.truncate(end);
		s.push('…');
	}
}

/// Blanks every IPv4 or IPv6 address in `text`, with or without a port.
///
/// Looks at each run of characters an address can be made of (hex digits,
/// `.`, `:`, `%`) and replaces it, or the part of it before a `:port`, when
/// it parses as an address. Version numbers with four parts look like IPv4
/// addresses and are blanked too, which is the safe side to err on.
#[must_use]
pub fn scrub_ips(text: &str) -> String {
	let is_addr_char = |c: char| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '%');
	let mut out = String::with_capacity(text.len());
	let mut rest = text;

	while let Some(start) = rest.find(is_addr_char) {
		out.push_str(&rest[..start]);
		let run_len = rest[start..]
			.find(|c: char| !is_addr_char(c))
			.unwrap_or(rest.len() - start);
		let run = &rest[start..start + run_len];
		out.push_str(&scrub_run(run));
		rest = &rest[start + run_len..];
	}

	out.push_str(rest);
	out
}

fn scrub_run(run: &str) -> String {
	if !run.contains(['.', ':']) || run.len() < 3 {
		return run.to_owned();
	}

	// Punctuation around it ("tcp:1.2.3.4", "addr: ", end of a sentence)
	// isn't part of it; "::1" is, so the untrimmed start is tried first.
	let core = run.trim_end_matches(['.', ':']);
	let tail = &run[core.len()..];
	if let Some(scrubbed) = scrub_core(core) {
		return format!("{scrubbed}{tail}");
	}

	// Glued to a word that ends in hex letters ("Failed:10.0.0.1"): try
	// what follows each separator.
	for (i, _) in core.match_indices(['.', ':']) {
		let (lead, inner) = core.split_at(i + 1);
		if let Some(scrubbed) = scrub_core(inner) {
			return format!("{lead}{scrubbed}{tail}");
		}
	}

	run.to_owned()
}

/// An address, or an address with a port, blanked; None if it's neither.
fn scrub_core(core: &str) -> Option<String> {
	if is_ip(core) {
		return Some(REDACTED.to_owned());
	}

	// "1.2.3.4:8448"
	let (addr, port) = core.rsplit_once(':')?;
	(!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && is_ip(addr))
		.then(|| format!("{REDACTED}:{port}"))
}

fn is_ip(s: &str) -> bool {
	// A zone ("fe80::1%eth0") is cut at the run's end; drop it to parse.
	let s = s.split('%').next().unwrap_or(s);
	let v6_shaped = s.contains("::") || s.matches(':').count() == 7;
	s.parse::<Ipv4Addr>().is_ok() || (v6_shaped && s.parse::<Ipv6Addr>().is_ok())
}

#[cfg(test)]
mod tests {
	use super::scrub_ips;

	#[test]
	fn blanks_addresses() {
		assert_eq!(scrub_ips("from 203.0.113.175 now"), "from [ip] now");
		assert_eq!(scrub_ips("peer 10.2.0.2:8449."), "peer [ip]:8449.");
		assert_eq!(scrub_ips("v6 2001:db8::1 and [::1]:80"), "v6 [ip] and [[ip]]:80");
		assert_eq!(scrub_ips("mapped ::ffff:1.2.3.4"), "mapped [ip]");
		assert_eq!(scrub_ips("full 2001:0db8:0000:0000:0000:ff00:0042:8329 x"), "full [ip] x");
		assert_eq!(scrub_ips("ip=10.20.30.40,"), "ip=[ip],");
		assert_eq!(scrub_ips(r#"["tcp:127.0.0.1:18009"]"#), r#"["tcp:[ip]:18009"]"#);
		assert_eq!(scrub_ips("at :10.0.0.1."), "at :[ip].");
		assert_eq!(scrub_ips("Failed:10.0.0.1"), "Failed:[ip]");
		assert_eq!(scrub_ips("bad:1.2.3.4:443 x"), "bad:[ip]:443 x");
		assert_eq!(scrub_ips("cafe.::1"), "cafe.[ip]");
	}

	#[test]
	fn keeps_other_text() {
		for text in [
			"at 12:30:45.123 sent 4 events",
			"room !abc:anoncat.gleeze.com cafe",
			"@user:matrix.org joined",
			"took 1.5s, 0.25 ms",
			"deadbeef::",
			"version 1.9.4",
		] {
			assert_eq!(scrub_ips(text), text);
		}
	}
}
