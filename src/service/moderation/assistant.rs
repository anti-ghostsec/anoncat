//! The panel's assistant: a Venice chat that knows the server's current
//! state and its real admin commands, and proposes commands for the admin
//! to run.
//!
//! Every user ID, room ID, alias and event ID in what it's sent becomes a
//! placeholder such as `@user-3` or `!room-1`. The table stays in memory
//! here, and the reply's placeholders are turned back into the real IDs, so
//! a command it proposes about `@user-3` runs on the right account.

use std::{collections::HashMap, fmt::Write as _};

use futures::StreamExt;
use regex::Regex;
use tuwunel_core::{
	Err, Result, implement,
	utils::sys::storage::{available_space, total_space},
};

use super::{IDS, Service, Turn, venice};

/// Placeholders the table holds before it starts over.
const MASKS_MAX: usize = 20_000;
/// Public rooms listed in the snapshot.
const ROOMS_LISTED: usize = 30;
/// Characters of history the assistant is sent.
const HISTORY_CHARS: usize = 60_000;

#[derive(Default)]
pub(super) struct Masks {
	to: HashMap<String, String>,
	from: HashMap<String, String>,
}

impl Masks {
	/// Placeholders for IDs, and IP addresses blanked: command output can
	/// hold a device's last address.
	fn mask(&mut self, text: &str) -> String {
		let text = crate::logs::scrub_ips(text);
		IDS.replace_all(&text, |c: &regex::Captures<'_>| {
			let id = &c[0];
			if let Some(mask) = self.to.get(id) {
				return mask.clone();
			}

			let kind = match id.as_bytes()[0] {
				| b'@' => "@user",
				| b'!' => "!room",
				| b'#' => "#alias",
				| b'$' => "$event",
				| b'm' => "+media",
				| _ => "+group",
			};
			let mask = format!("{kind}-{}", self.to.len().saturating_add(1));
			self.to.insert(id.to_owned(), mask.clone());
			self.from.insert(mask.clone(), id.to_owned());
			mask
		})
		.into_owned()
	}

	fn unmask(&self, text: &str) -> String {
		use std::sync::LazyLock;

		static MASKS: LazyLock<Regex> = LazyLock::new(|| {
			Regex::new(r"[@!#$+](?:user|room|alias|event|group|media)-[0-9]+\b").expect("valid regex")
		});

		MASKS
			.replace_all(text, |c: &regex::Captures<'_>| {
				self.from
					.get(&c[0])
					.cloned()
					.unwrap_or_else(|| c[0].to_owned())
			})
			.into_owned()
	}
}

/// Answers the admin. `history` holds the chat so far, including command
/// output the panel added after the admin ran something.
#[implement(Service)]
pub async fn assistant(&self, history: &[Turn]) -> Result<String> {
	let Some(key) = self.key() else {
		return Err!(Request(NotFound("No Venice API key is saved yet.")));
	};

	let settings = self.settings();
	let model = self.model(&settings.text_model, false).await?;
	let snapshot = self.snapshot().await?;
	let commands = self.command_list();

	let system = format!(
		"You help the admin of a small, privacy-focused Matrix homeserver (anoncat, built on \
		 tuwunel) run it from its admin panel. Be brief and plain.\n\nYou can't act or look \
		 anything up yourself. What you know is the snapshot below, taken just now, plus \
		 command output the admin shares in the chat. When you need more, or when a command \
		 would do what the admin asks, put each command alone in a fenced block marked \
		 command, without the !admin prefix:\n```command\nusers list-users\n```\nThe admin \
		 clicks Run to run it and its output comes back to you. Never claim you ran \
		 anything. Use only commands from the list below. Say plainly when a command \
		 deletes data or can't be undone.\n\nPeople, rooms and events appear as placeholders \
		 like @user-3, !room-1, #alias-2. Use them exactly as written, as complete IDs; \
		 they're turned back into the real ones for the admin.\n\n# Server now\n\
		 {snapshot}\n# Admin commands\n{commands}"
	);

	// One table for the whole request: a placeholder in the system prompt
	// and the same one in the reply always name the same thing.
	let (system, turns) = {
		let mut masks = self.masks.lock().expect("locked");
		if masks.to.len() > MASKS_MAX {
			*masks = Masks::default();
		}
		let system = masks.mask(&system);
		let mut budget = HISTORY_CHARS;
		let mut turns: Vec<Turn> = history
			.iter()
			.rev()
			.take_while(|t| {
				let len = t.content.len();
				let fits = len <= budget;
				budget = budget.saturating_sub(len);
				fits
			})
			.map(|t| Turn { role: t.role.clone(), content: masks.mask(&t.content) })
			.collect();
		turns.reverse();
		(system, turns)
	};

	let reply =
		venice::chat(self.client(), &key, &model, &system, &turns, Vec::new()).await?;

	Ok(self.masks.lock().expect("locked").unmask(&reply))
}

/// The server's state, as the assistant is told it.
#[implement(Service)]
async fn snapshot(&self) -> Result<String> {
	let services = &self.services;
	let mut out = String::new();

	let uptime = services
		.server
		.started
		.elapsed()
		.map_or(0, |d| d.as_secs());
	writeln!(out, "- Software: anoncat {}", tuwunel_core::version::version())?;
	writeln!(out, "- Up for {}h {}m", uptime / 3600, (uptime % 3600) / 60)?;

	let path = &services.config.database_path;
	if let (Ok(total), Ok(free)) = (total_space(path), available_space(path)) {
		writeln!(out, "- Disk: {} GiB free of {} GiB", free >> 30, total >> 30)?;
	}

	let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
	let field = |name: &str| {
		meminfo
			.lines()
			.find_map(|l| l.strip_prefix(name))
			.and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
	};
	if let (Some(total), Some(free)) = (field("MemTotal:"), field("MemAvailable:")) {
		writeln!(out, "- RAM: {} GiB free of {} GiB", free >> 20, total >> 20)?;
	}

	let users = services.users.list_local_users().count().await;
	let rooms = services.metadata.iter_ids().count().await;
	writeln!(out, "- Accounts here: {users}; rooms this server knows: {rooms}")?;

	let config = &services.config;
	writeln!(
		out,
		"- Registration: {}; federation: {}",
		if config.allow_registration { "open with a token" } else { "closed" },
		if config.allow_federation { "on" } else { "off" },
	)?;
	writeln!(out, "- Cleanup limits: {}", serde_json::to_string(&services.cleanup.limits())?)?;

	let public: Vec<_> = services
		.directory
		.public_rooms()
		.map(ToOwned::to_owned)
		.collect()
		.await;
	writeln!(out, "- Public rooms listed here ({}):", public.len())?;
	for room in public.iter().take(ROOMS_LISTED) {
		let name = services
			.state_accessor
			.get_name(room)
			.await
			.unwrap_or_default();
		let members = services
			.state_cache
			.room_joined_count(room)
			.await
			.unwrap_or(0);
		let encrypted = services.state_accessor.is_encrypted_room(room).await;
		writeln!(
			out,
			"  - {room} \"{name}\": {members} members{}",
			if encrypted { ", encrypted" } else { "" }
		)?;
	}

	let s = &self.settings();
	writeln!(
		out,
		"- AI moderation: {}; images it judges clearly bad: {}; watching the public list: {}, \
		 any public room joined: {}, extra rooms: {}",
		if s.enabled { "on" } else { "off" },
		if s.media_mode == super::MediaMode::Hide { "hidden until review" } else { "only reported" },
		s.watch_directory,
		s.watch_public_joined,
		s.extra_rooms.len(),
	)?;

	let reports = self.reports(false, 20).await;
	writeln!(out, "- Open moderation reports ({}):", reports.len())?;
	for r in &reports {
		writeln!(
			out,
			"  - report {} ({}): {} {} by {} in {}, {}, suggests {}{}: {}",
			r.id,
			r.source,
			r.severity,
			r.category,
			r.sender,
			r.room_id,
			r.kind,
			r.suggested,
			if r.hidden { ", media hidden" } else { "" },
			r.reason
		)?;
	}

	let notices = services.admin.notices(10).await;
	writeln!(out, "- Latest server notices:")?;
	for n in &notices {
		let body: String = n.body.chars().take(300).collect();
		writeln!(out, "  - {}", body.replace('\n', " "))?;
	}

	Ok(out)
}

/// Every admin command with its one-line description, from the command
/// tree itself.
#[implement(Service)]
fn command_list(&self) -> String {
	let Some(root) = self
		.services
		.admin
		.command
		.read()
		.expect("locked")
		.clone()
	else {
		return "(the command list isn't available)".into();
	};

	let mut out = String::new();
	walk(&root.clap(), "", &mut out);
	out
}

fn walk(command: &clap::Command, prefix: &str, out: &mut String) {
	for sub in command.get_subcommands() {
		let name = sub.get_name();
		if name == "help" || sub.is_hide_set() {
			continue;
		}

		let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix} {name}") };
		let about = sub
			.get_about()
			.map(|a| a.to_string())
			.unwrap_or_default();
		let about = about.lines().next().unwrap_or_default();

		if sub.has_subcommands() {
			let _ = writeln!(out, "{path}: {about}");
			walk(sub, &path, out);
		} else {
			let args: String = sub
				.get_arguments()
				.filter(|a| !a.is_hide_set() && a.get_id() != "help")
				.map(|a| match (a.get_long(), a.is_positional()) {
					| (Some(long), _) => format!(" [--{long}]"),
					| (None, true) => format!(" <{}>", a.get_id()),
					| _ => String::new(),
				})
				.collect();
			let _ = writeln!(out, "  {path}{args} — {about}");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::Masks;

	#[test]
	fn placeholders_turn_back_into_ids() {
		let mut masks = Masks::default();
		let sent = masks.mask("deactivate @bob:anoncat.gleeze.com, he spams !abc:x.org and @bob:anoncat.gleeze.com from 203.0.113.175");
		assert_eq!(sent, "deactivate @user-1, he spams !room-2 and @user-1 from [ip]");
		assert_eq!(
			masks.unmask("```command\nusers deactivate @user-1\n``` and @user-99"),
			"```command\nusers deactivate @bob:anoncat.gleeze.com\n``` and @user-99"
		);
	}
}
