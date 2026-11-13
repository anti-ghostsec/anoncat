//! Venice AI's chat API (OpenAI-shaped), as the moderation uses it.
//!
//! Requests leave through the server's own network, which is the VPN tunnel;
//! no address or name of this machine goes with them, only the API key and
//! what is being checked (see the README for what that can be). The user agent
//! is plain so it doesn't name the server software.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result, err};

const API: &str = "https://api.venice.ai/api/v1";
const TIMEOUT: Duration = Duration::from_secs(90);
const USER_AGENT: &str = "Mozilla/5.0";

/// One model Venice offers, as the settings page lists it.
#[derive(Clone, Debug, Serialize)]
pub struct Model {
	pub id: String,
	pub vision: bool,
	/// Venice's own picks: "default", "default_vision", ...
	pub traits: Vec<String>,
	/// USD per million input tokens.
	pub price_in: Option<f64>,
}

/// Venice's model list. Needs no key.
pub(super) async fn models(client: &reqwest::Client) -> Result<Vec<Model>> {
	let res = client
		.get(format!("{API}/models?type=text"))
		.header("User-Agent", USER_AGENT)
		.timeout(TIMEOUT)
		.send()
		.await
		.map_err(|e| err!("AI service unreachable: {e}"))?;

	let body: Value = res
		.json()
		.await
		.map_err(|e| err!("AI service model list unreadable: {e}"))?;

	let models = body["data"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|m| {
			let spec = &m["model_spec"];
			Some(Model {
				id: m["id"].as_str()?.to_owned(),
				vision: spec["capabilities"]["supportsVision"]
					.as_bool()
					.unwrap_or(false),
				traits: spec["traits"]
					.as_array()
					.into_iter()
					.flatten()
					.filter_map(|t| t.as_str().map(ToOwned::to_owned))
					.collect(),
				price_in: spec["pricing"]["input"]["usd"].as_f64(),
			})
		})
		.collect();

	Ok(models)
}

/// A chat message part: text, or an image as a data: URL.
pub(super) enum Part {
	Text(String),
	Image { mime: String, data: Vec<u8> },
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Turn {
	pub role: String,
	pub content: String,
}

/// Asks `model` with a system prompt and the user's parts; returns the reply
/// text.
pub(super) async fn chat(
	client: &reqwest::Client,
	key: &str,
	model: &str,
	system: &str,
	history: &[Turn],
	parts: Vec<Part>,
) -> Result<String> {
	use base64::{Engine as _, engine::general_purpose::STANDARD};

	let content: Vec<Value> = parts
		.into_iter()
		.map(|part| match part {
			| Part::Text(text) => json!({ "type": "text", "text": text }),
			| Part::Image { mime, data } => json!({
				"type": "image_url",
				"image_url": { "url": format!("data:{mime};base64,{}", STANDARD.encode(data)) },
			}),
		})
		.collect();

	let mut messages = vec![json!({ "role": "system", "content": system })];
	messages.extend(
		history
			.iter()
			.filter(|t| matches!(t.role.as_str(), "user" | "assistant"))
			.map(|t| json!({ "role": t.role, "content": t.content })),
	);
	if !content.is_empty() {
		messages.push(json!({ "role": "user", "content": content }));
	}

	let body = json!({
		"model": model,
		"messages": messages,
		"temperature": 0,
		"venice_parameters": {
			"include_venice_system_prompt": false,
			"enable_web_search": "off",
			"strip_thinking_response": true,
		},
	});

	let res = client
		.post(format!("{API}/chat/completions"))
		.header("User-Agent", USER_AGENT)
		.bearer_auth(key)
		.json(&body)
		.timeout(TIMEOUT)
		.send()
		.await
		.map_err(|e| err!("AI service unreachable: {e}"))?;

	let status = res.status();
	let reply: Value = res
		.json()
		.await
		.map_err(|e| err!("AI service reply unreadable ({status}): {e}"))?;

	if !status.is_success() {
		let message = reply["error"]["message"]
			.as_str()
			.or_else(|| reply["error"].as_str())
			.unwrap_or("no detail");
		return Err!("AI service refused ({status}): {message}");
	}

	reply["choices"][0]["message"]["content"]
		.as_str()
		.map(strip_thinking)
		.ok_or_else(|| err!("AI service reply had no text"))
}

/// Drops a reasoning model's `<think>` block.
fn strip_thinking(text: &str) -> String {
	match text.rfind("</think>") {
		| Some(end) => text[end.saturating_add("</think>".len())..].trim().to_owned(),
		| None => text.trim().to_owned(),
	}
}

/// The JSON object in a reply that may wrap it in prose or a code fence.
pub(super) fn json_in(text: &str) -> Option<Value> {
	let start = text.find('{')?;
	let end = text.rfind('}')?;
	serde_json::from_str(text.get(start..=end)?).ok()
}

#[cfg(test)]
mod tests {
	use super::{json_in, strip_thinking};

	#[test]
	fn replies_are_read_through_fences_and_thinking() {
		let reply = strip_thinking("<think>hmm {no}</think>\n```json\n{\"flags\": []}\n```");
		assert_eq!(json_in(&reply), Some(serde_json::json!({ "flags": [] })));
		assert_eq!(json_in("nothing here"), None);
	}
}
