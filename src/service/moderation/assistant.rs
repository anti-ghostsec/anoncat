//! The panel's assistant: a Venice chat that knows the server's current
//! state and its real admin commands. Commands that only look things up
//! (`READ_ONLY`) it runs itself, a few rounds at most, reading their output
//! before it answers; anything else it proposes for the admin to run.
//!
//! Every user ID, room ID, alias and event ID in what it's sent becomes a
//! placeholder such as `@user-3` or `!room-1`. The table stays in memory
//! here, and the reply's placeholders are turned back into the real IDs, so
//! a command it proposes about `@user-3` runs on the right account.

use std::{
	collections::HashMap,
	fmt::Write as _,
	sync::LazyLock,
	time::{Duration, Instant},
};

use futures::StreamExt;
use regex::Regex;
use ruma::UserId;
use serde::Serialize;
use tuwunel_core::{
	Err, Result, implement,
	utils::sys::storage::{available_space, total_space},
};

use super::{IDS, Service, Turn, venice};
use crate::admin::CommandInput;

/// Placeholders the table holds before it starts over.
const MASKS_MAX: usize = 20_000;
/// Public rooms listed in the snapshot.
const ROOMS_LISTED: usize = 30;
/// Characters of history the assistant is sent.
const HISTORY_CHARS: usize = 60_000;
/// Rounds of commands the assistant runs itself before it must answer.
const AUTO_ROUNDS: usize = 6;
/// Commands it runs itself in one round.
const AUTO_PER_ROUND: usize = 3;
/// No new round starts after this; the panel's request gives up at 240 s.
const AUTO_TIME: Duration = Duration::from_secs(110);
/// Characters of a command's output it reads.
const OUTPUT_CHARS: usize = 6000;

/// Commands that only read, which the assistant runs without asking. None
/// of them changes anything or prints a secret. Several take a room as an
/// alias, and another server's alias is looked up by asking that server, so
/// a command naming one waits for the admin (see `is_read_only`): text the
/// assistant reads, like a room's name or a message, could otherwise steer
/// this server into contacting a server of someone else's choosing.
const READ_ONLY: &[&str] = &[
	"rooms list",
	"rooms info",
	"rooms list-joined-members",
	"rooms exists",
	"rooms list-extremities",
	"rooms directory list",
	"rooms moderation list-banned-rooms",
	"rooms alias which",
	"rooms alias list",
	"users list-users",
	"users list-joined-rooms",
	"users last-active",
	"server uptime",
	"server memory-usage",
	"server list-backups",
	"media get-file-info",
	"media where-posted",
	"federation incoming-federation",
	"federation remote-user-in-rooms",
	"query room-timeline",
	"query room-state-cache",
	"query room-alias resolve-local-alias",
	"query room-alias local-aliases-for-room",
	"query room-alias all-local-aliases",
	"debug get-pdu",
	"debug get-room-state",
	"debug first-pdu-in-room",
	"debug latest-pdu-in-room",
];

/// One step of an answer, as the panel shows it: something the assistant
/// said, or a command it ran and that command's output.
#[derive(Clone, Debug, Serialize)]
pub struct Said {
	pub role: String,
	/// What goes back into the chat history.
	pub content: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub command: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub output: Option<String>,
	/// A reply whose commands all ran by themselves.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub ran: bool,
}

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
				| b'm' => "mxc://media",
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
		static MASKS: LazyLock<Regex> = LazyLock::new(|| {
			Regex::new(r"[@!#$+](?:user|room|alias|event|group)-[0-9]+\b").expect("valid regex")
		});
		// Media is written as mxc://media-7, but models drop the scheme, add
		// a second one, or use the old +media-7: any of them is that file.
		static MEDIA: LazyLock<Regex> = LazyLock::new(|| {
			Regex::new(r"(?:mxc://)*\+?\bmedia-([0-9]+)\b").expect("valid regex")
		});

		let text = MASKS.replace_all(text, |c: &regex::Captures<'_>| {
			self.from
				.get(&c[0])
				.cloned()
				.unwrap_or_else(|| c[0].to_owned())
		});

		MEDIA
			.replace_all(&text, |c: &regex::Captures<'_>| {
				self.from
					.get(&format!("mxc://media-{}", &c[1]))
					.cloned()
					.unwrap_or_else(|| c[0].to_owned())
			})
			.into_owned()
	}
}

/// Answers the admin. `history` holds the chat so far, including command
/// output the panel added after the admin ran something. Read-only commands
/// the assistant asks for run as `admin`.
#[implement(Service)]
pub async fn assistant(&self, history: &[Turn], admin: &UserId) -> Result<Vec<Said>> {
	let Some(key) = self.key() else {
		return Err!(Request(NotFound("No AI service key is saved yet.")));
	};

	let settings = self.settings();
	let model = self.model(&settings.text_model, false).await?;
	let snapshot = self.snapshot().await?;
	let commands = self.command_list();
	let read_only = READ_ONLY.join("\n");

	let system = format!(
		"You help the admin of a small, privacy-focused Matrix homeserver (anoncat, built on \
		 tuwunel) run it from its admin panel. Be brief and plain.\n\nWhat you know is the \
		 snapshot below, taken just now, plus command output in the chat. To look something \
		 up, or when a command would do what the admin asks, put each command alone in a \
		 fenced block marked command, without the !admin prefix:\n```command\nusers \
		 list-users\n```\nCommands that start like one in the read-only list below run by \
		 themselves at once, and their output comes back to you as the next message: use them \
		 freely to find out what you need, up to {AUTO_ROUNDS} rounds, instead of asking the \
		 admin to run them, and only answer once you know. One that names another server's \
		 #alias waits for the admin, since looking that alias up means asking that server; \
		 use the room ID instead. A reply that mixes read-only commands with others runs \
		 none of them. Every other command waits for the admin to \
		 click Run. Don't use tool or function calls of any kind, DSML included: a command block is the \
		 only way a command runs. Never claim something changed unless a command's output says so. Use \
		 only commands from the list below. Say plainly when a command deletes data or can't \
		 be undone.\n\nPeople, rooms and events appear as placeholders like @user-3, \
		 !room-1, #alias-2, and files as mxc://media-4. Use them exactly as written, as \
		 complete IDs; they're turned back into the real ones before anything runs.\n\nWho \
		 sent something is known only from a report's sender or an event's sender in command \
		 output that shows that very event. Never work it out from timing, from which server \
		 a file is on, or from who spoke last: name nobody, and say it can't be told. In an \
		 encrypted room the server can't read which file a message carries. A file on another \
		 server was uploaded by someone there, not necessarily by anyone in the room.\n\nWhen \
		 something bad was posted, don't stop at who sent it: say what can be done, and put the \
		 commands for it in command blocks. Run media where-posted on the file first: it names \
		 the uploader of a file from here, and the messages carrying it with their senders. \
		 Then, depending on what it finds: media swap <mxc> [reason] takes the file away from \
		 everyone for good, whoever sent it; media hide <mxc> holds it for review; \
		 users redact-event <event> removes a message sent from here; federation block \
		 <@user:server or server> [reason] stops a person or a whole server; users deactivate \
		 ends an account here. If the sender can't be found because the room is encrypted, \
		 swapping the file still works, and asking someone in the room to report the message \
		 from their app names the sender.\n\n# Server now\n{snapshot}\n# Read-only \
		 commands\n{read_only}\n\n# Admin commands\n{commands}"
	);

	let started = Instant::now();
	let mut history = history.to_vec();
	let mut said = Vec::new();
	for round in 0..=AUTO_ROUNDS {
		let reply = self.ask(&key, &model, &system, &history).await?;
		history.push(Turn { role: "assistant".into(), content: reply.clone() });

		let proposed = proposed_commands(&reply);
		let runs = !proposed.is_empty()
			&& proposed
				.iter()
				.all(|c| is_read_only(c, self.services.globals.server_name().as_str()))
			&& round < AUTO_ROUNDS
			&& started.elapsed() < AUTO_TIME;

		said.push(Said {
			role: "assistant".into(),
			content: reply,
			command: None,
			output: None,
			ran: runs,
		});
		if !runs {
			break;
		}

		for command in proposed.into_iter().take(AUTO_PER_ROUND) {
			let output = self.run_read_only(&command, admin).await;
			let content =
				format!("The server ran `{command}` for you. Output:\n```\n{output}\n```");
			history.push(Turn { role: "user".into(), content: content.clone() });
			said.push(Said {
				role: "user".into(),
				content,
				command: Some(command),
				output: Some(output),
				ran: false,
			});
		}
	}

	Ok(said)
}

/// One request to Venice with everything masked, and the reply unmasked.
#[implement(Service)]
async fn ask(&self, key: &str, model: &str, system: &str, history: &[Turn]) -> Result<String> {
	// One table for the whole request: a placeholder in the system prompt
	// and the same one in the reply always name the same thing.
	let (system, turns) = {
		let mut masks = self.masks.lock().expect("locked");
		if masks.to.len() > MASKS_MAX {
			*masks = Masks::default();
		}
		let system = masks.mask(system);
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

	let reply = venice::chat(self.client(), key, model, &system, &turns, Vec::new()).await?;

	let reply = self.masks.lock().expect("locked").unmask(&reply);

	Ok(from_dsml(&reply))
}

/// A tag of DeepSeek's tool-call markup (DSML), with either bar it's
/// written with: `<｜DSML｜name` or `</｜DSML｜name`.
const DSML: &str = r"<\s*/?\s*[｜|]\s*DSML\s*[｜|]\s*";

/// DeepSeek models were trained to call tools in their own markup, and
/// write it even when told to use ```command blocks:
///
/// ```text
/// <｜DSML｜function_calls>
/// <｜DSML｜invoke name="run_command">
/// <｜DSML｜parameter name="command" string="true">users list-users</｜DSML｜parameter>
/// </｜DSML｜invoke>
/// </｜DSML｜function_calls>
/// ```
///
/// Each invoke becomes a ```command block, so the panel, the chat history
/// and the read-only check all see one form. The command is the parameter
/// named like one, else the only one there is; an invoke without one is
/// dropped rather than guessed at.
fn from_dsml(reply: &str) -> String {
	static INVOKE: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(&format!(r#"(?s){DSML}invoke\s+name\s*=\s*"[^"]*"\s*>(.*?){DSML}invoke\s*>"#))
			.expect("valid regex")
	});
	static PARAM: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(&format!(
			r#"(?s){DSML}parameter\s+name\s*=\s*"([^"]*)"[^>]*>(.*?){DSML}parameter\s*>"#
		))
		.expect("valid regex")
	});
	static WRAPPER: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(&format!(r"{DSML}(function_calls|tool_calls)\s*>")).expect("valid regex")
	});

	if !reply.contains("DSML") {
		return reply.to_owned();
	}

	let replaced = INVOKE.replace_all(reply, |invoke: &regex::Captures<'_>| {
		let params: Vec<(String, String)> = PARAM
			.captures_iter(&invoke[1])
			.map(|p| (p[1].to_lowercase(), p[2].trim().to_owned()))
			.collect();
		let command = params
			.iter()
			.find(|(name, _)| ["command", "cmd", "admin_command", "input", "query"].contains(&name.as_str()))
			.or_else(|| (params.len() == 1).then(|| &params[0]))
			.map(|(_, value)| value.lines().next().unwrap_or_default().trim().to_owned())
			.filter(|command| !command.is_empty());

		command.map_or_else(String::new, |command| format!("\n```command\n{command}\n```\n"))
	});

	WRAPPER.replace_all(&replaced, "").trim().to_owned()
}

#[implement(Service)]
async fn run_read_only(&self, command: &str, admin: &UserId) -> String {
	let input = CommandInput {
		command: command.to_owned(),
		reply_id: None,
		sender: Some(admin.to_owned()),
	};

	let output = match self.services.admin.command_in_place(input).await {
		| Ok(output) => output
			.map(|o| o.as_str().to_owned())
			.unwrap_or_else(|| "Done, no output.".into()),
		| Err(output) => format!("Failed: {}", output.as_str()),
	};

	match output.char_indices().nth(OUTPUT_CHARS) {
		| Some((cut, _)) => format!("{}\n… (cut)", &output[..cut]),
		| None => output,
	}
}

/// The commands in a reply's ```command blocks.
fn proposed_commands(reply: &str) -> Vec<String> {
	static BLOCK: LazyLock<Regex> =
		LazyLock::new(|| Regex::new(r"```command[ \t]*\n([\s\S]*?)```").expect("valid regex"));

	BLOCK
		.captures_iter(reply)
		.map(|c| {
			let command = c[1].trim();
			command
				.strip_prefix("!admin ")
				.unwrap_or(command)
				.trim()
				.to_owned()
		})
		.filter(|c| !c.is_empty())
		.collect()
}

/// Whether a command is one line starting with a read-only command's words,
/// naming no other server's alias: this server's own aliases resolve from its
/// database, but another server's is looked up by asking that server.
fn is_read_only(command: &str, our_server: &str) -> bool {
	if command.contains('\n') {
		return false;
	}

	let words: Vec<&str> = command.split_whitespace().collect();
	let elsewhere = words.iter().any(|word| {
		word.trim_matches(['"', '\'', '`'])
			.strip_prefix('#')
			.is_some_and(|alias| alias.split_once(':').is_none_or(|(_, server)| server != our_server))
	});
	if elsewhere {
		return false;
	}

	READ_ONLY.iter().any(|allowed| {
		let allowed: Vec<&str> = allowed.split(' ').collect();
		words.len() >= allowed.len() && words[..allowed.len()] == allowed[..]
	})
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
		match (config.allow_registration, config.web_signup) {
			| (false, _) => "closed",
			| (true, false) => "apps need a token",
			| (true, true) => "anyone through the web sign-up page with a picture captcha; apps need a token",
		},
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
			"  - report {} (found by {}): {} {}, posted by {} in {}, event {}, {}{}, suggests {}{}: {}",
			r.id,
			r.source,
			r.severity,
			r.category,
			if r.sender.is_empty() { "unknown" } else { &r.sender },
			r.room_id,
			if r.event_id.is_empty() { "unknown" } else { &r.event_id },
			r.kind,
			r.mxc.as_deref().map(|m| format!(" {m}")).unwrap_or_default(),
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
	use super::{Masks, is_read_only, proposed_commands};

	#[test]
	fn only_reads_run_by_themselves() {
		let reply = "Let me look.\n```command\n!admin query room-timeline last !r:x\n```\nand\n```command\nusers deactivate @a:x\n```";
		let proposed = proposed_commands(reply);
		assert_eq!(proposed, ["query room-timeline last !r:x", "users deactivate @a:x"]);
		let dsml = "Checking.\n<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"run_command\">\n<｜DSML｜parameter name=\"command\" string=\"true\">users list-users</｜DSML｜parameter>\n</｜DSML｜invoke>\n<|DSML|invoke name=\"x\"><|DSML|parameter name=\"text\">rooms info !r:x</|DSML|parameter></|DSML|invoke>\n</｜DSML｜function_calls>";
		let normal = super::from_dsml(dsml);
		assert_eq!(proposed_commands(&normal), ["users list-users", "rooms info !r:x"]);
		assert!(!normal.contains("DSML"));
		assert_eq!(super::from_dsml("no markup"), "no markup");

		let ro = |c: &str| is_read_only(c, "x");
		assert!(ro(&proposed[0]));
		assert!(!ro(&proposed[1]));
		assert!(ro("rooms info !r:x"));
		assert!(ro("rooms info !r:elsewhere.org"));
		assert!(ro("rooms info #lobby:x"));
		assert!(!ro("rooms info #lobby:evil.example"));
		assert!(!ro("debug get-room-state '#a:evil.example'"));
		assert!(!ro("query room-alias resolve-alias #a:x"));
		assert!(ro("query room-alias resolve-local-alias #a:x"));
		assert!(!ro("rooms delete !r:x"));
		assert!(!ro("rooms listing"));
		assert!(!ro("rooms info !r:x\nsecond line"));
		assert!(!ro("server show-config"));
	}

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

	#[test]
	fn media_placeholders_turn_back_however_written() {
		let mut masks = Masks::default();
		assert_eq!(masks.mask("who sent mxc://xmr.example/AbC_d-9"), "who sent mxc://media-1");
		for written in ["mxc://media-1", "media-1", "+media-1", "mxc://mxc://media-1", "mxc://+media-1"] {
			assert_eq!(
				masks.unmask(&format!("media get-file-info {written}")),
				"media get-file-info mxc://xmr.example/AbC_d-9",
				"{written}"
			);
		}
	}
}
