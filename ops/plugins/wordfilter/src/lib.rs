//! Example anoncat plugin.
//!
//! - `check-event`: refuses messages that contain any of the configured words.
//! - `on-event`: counts messages per room in the plugin's key-value storage.
//! - `on-command`: `stats <room>` shows a room's count; `words` lists the
//!   blocked words.
//!
//! Build: `cargo build --release --target wasm32-wasip2`, then copy
//! `target/wasm32-wasip2/release/wordfilter.wasm` and `wordfilter.toml` into
//! the server's plugins directory and run `!admin plugins load wordfilter`.

use std::cell::RefCell;

use serde::Deserialize;

wit_bindgen::generate!({
	path: "../../../src/service/plugins/wit",
	world: "plugin",
});

// The world's `use types.{event, verdict}` already brings `Event` and
// `Verdict` into scope.
use anoncat::plugin::host::{self, Level};

#[derive(Default, Deserialize)]
struct Config {
	#[serde(default)]
	words: Vec<String>,
}

thread_local! {
	static WORDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[derive(Deserialize)]
struct Content {
	#[serde(default)]
	body: String,
}

struct WordFilter;

impl Guest for WordFilter {
	fn init(config: String) -> Result<(), String> {
		let config: Config = serde_json::from_str(&config).map_err(|e| format!("bad config: {e}"))?;
		let words: Vec<String> = config.words.iter().map(|w| w.to_lowercase()).collect();
		host::log(Level::Info, &format!("blocking {} words", words.len()));
		WORDS.with(|w| *w.borrow_mut() = words);
		Ok(())
	}

	fn check_event(event: Event) -> Verdict {
		if event.kind != "m.room.message" {
			return Verdict::Allow;
		}

		let Ok(content) = serde_json::from_str::<Content>(&event.content) else {
			return Verdict::Allow;
		};

		let body = content.body.to_lowercase();
		let blocked = WORDS.with(|w| w.borrow().iter().any(|word| body.contains(word.as_str())));

		if blocked {
			Verdict::Reject("That message contains a word this server doesn't allow.".into())
		} else {
			Verdict::Allow
		}
	}

	fn on_event(event: Event) {
		let key = format!("count:{}", event.room_id);
		let count = host::kv_get(&key)
			.and_then(|bytes| bytes.try_into().ok())
			.map(u64::from_le_bytes)
			.unwrap_or(0);

		host::kv_set(&key, &(count + 1).to_le_bytes());
	}

	fn on_command(args: Vec<String>) -> String {
		match args.first().map(String::as_str) {
			| Some("stats") => {
				let Some(room) = args.get(1) else {
					return "usage: stats <room id>".into();
				};
				let count = host::kv_get(&format!("count:{room}"))
					.and_then(|bytes| bytes.try_into().ok())
					.map(u64::from_le_bytes)
					.unwrap_or(0);
				format!("{count} messages seen in {room} since the plugin was loaded")
			},
			| Some("words") => WORDS.with(|w| format!("blocked: {}", w.borrow().join(", "))),
			| _ => "commands: stats <room id>, words".into(),
		}
	}
}

export!(WordFilter);
