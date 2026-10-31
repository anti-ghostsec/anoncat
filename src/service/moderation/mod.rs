//! AI moderation of public rooms, run through Venice AI.
//!
//! New messages in watched rooms are collected for a few minutes and checked
//! together; images are checked one at a time; files over the size limit are
//! reported without asking the AI. What the AI flags lands in the panel's
//! Reports queue with a suggested action, which an admin approves or
//! dismisses. On its own the AI may at most hide an image it judged clearly
//! bad, and only when the settings allow it; it never deletes anything.
//!
//! Only rooms that aren't encrypted are seen at all (encrypted ones carry
//! nothing readable). Before anything is sent, user IDs, room IDs and
//! aliases are replaced with placeholders such as U1 and R1; the mapping
//! stays here.

mod venice;

use std::{
	collections::HashMap,
	fmt::Write as _,
	sync::{Arc, Mutex, RwLock},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::StreamExt;
use regex::Regex;
use ruma::{
	Mxc, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomOrAliasId,
	events::{TimelineEventType, room::join_rules::JoinRule},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tuwunel_core::{
	Err, Result, debug, err, implement,
	utils::{stream::TryIgnore, time::now_millis},
	warn,
};
use tuwunel_database::Map;
use tuwunel_matrix::{Event, PduEvent, pdu::MAX_PDU_BYTES};

pub use self::venice::{Model, Turn};
use self::venice::{Part, json_in};

/// Messages waiting before a batch is checked early.
const BATCH_MAX: usize = 40;
/// Events waiting to be looked at; past this new ones are skipped.
const QUEUE: usize = 2000;
/// Images checked per batch; the rest wait for the next.
const IMAGES_PER_BATCH: usize = 10;
/// Images larger than this aren't sent to the AI.
const IMAGE_MAX_BYTES: u64 = 8 << 20;
/// Characters of a message the AI sees and the report keeps.
const EXCERPT: usize = 1000;
/// Reports kept; the oldest closed ones go first.
const REPORTS_KEPT: usize = 5000;
/// How long Venice's model list is reused.
const MODELS_TTL: Duration = Duration::from_secs(60 * 60);
const SETTINGS_KEY: &[u8] = b"moderation";

const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	settings_map: Arc<Map>,
	reports_map: Arc<Map>,
	settings: RwLock<Settings>,
	tx: mpsc::Sender<Item>,
	rx: Mutex<Option<mpsc::Receiver<Item>>>,
	models: Mutex<Option<(Instant, Vec<Model>)>>,
	/// Keeps report IDs unique within a millisecond.
	last_id: Mutex<u64>,
}

/// Whether the AI may hide an image it judges clearly bad until an admin
/// looks, or only report it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaMode {
	#[default]
	Suggest,
	Hide,
}

/// The moderation settings, edited on the panel's Settings page.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
	/// Master switch.
	pub enabled: bool,
	pub media_mode: MediaMode,
	/// Rooms listed in this server's public room directory.
	pub watch_directory: bool,
	/// Any room anyone can join that someone here is in, wherever it lives.
	pub watch_public_joined: bool,
	/// More rooms, by ID (aliases are resolved when saved). Rooms on other
	/// servers are seen once someone here has joined them.
	pub extra_rooms: Vec<OwnedRoomId>,
	pub check_messages: bool,
	pub check_images: bool,
	/// Files at least this large are reported for a look, without the AI.
	/// 0 turns that off.
	pub large_file_mb: u64,
	/// "auto" uses Venice's current default.
	pub text_model: String,
	pub vision_model: String,
	/// The server's rules, as the AI is told them.
	pub rules: String,
	/// How long new messages collect before they're checked together.
	pub batch_minutes: u64,
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			enabled: false,
			media_mode: MediaMode::Suggest,
			watch_directory: true,
			watch_public_joined: false,
			extra_rooms: Vec::new(),
			check_messages: true,
			check_images: true,
			large_file_mb: 100,
			text_model: "auto".into(),
			vision_model: "auto".into(),
			rules: DEFAULT_RULES.into(),
			batch_minutes: 2,
		}
	}
}

const DEFAULT_RULES: &str = "\
- No spam, scams, phishing or advertising floods.
- No threats, harassment or hate aimed at people.
- No posting anyone's private details (doxxing).
- No sexual content involving minors, ever.
- No gore or sexual content in rooms not marked for it.
- No selling illegal goods or services.";

/// One thing the moderation wants an admin to look at.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
	pub id: u64,
	pub ts: u64,
	/// "message", "image" or "large_file".
	pub kind: String,
	pub room_id: String,
	pub room_name: Option<String>,
	pub event_id: String,
	pub sender: String,
	pub mxc: Option<String>,
	pub excerpt: String,
	pub category: String,
	/// "low", "medium" or "high".
	pub severity: String,
	pub reason: String,
	/// "redact", "hide" or "review".
	pub suggested: String,
	/// Whether the media is hidden now.
	pub hidden: bool,
	/// "open", "dismissed" or "resolved".
	pub status: String,
	/// "ai" or "size".
	pub source: String,
}

/// A watched event, as queued for a batch.
struct Item {
	event_id: OwnedEventId,
	room_id: OwnedRoomId,
	sender: OwnedUserId,
	content: Value,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let (tx, rx) = mpsc::channel(QUEUE);
		Ok(Arc::new(Self {
			services: args.services.clone(),
			settings_map: args.db["anoncat_settings"].clone(),
			reports_map: args.db["anoncat_reports"].clone(),
			settings: RwLock::new(Settings::default()),
			tx,
			rx: Mutex::new(Some(rx)),
			models: Mutex::new(None),
			last_id: Mutex::new(0),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		if let Ok(stored) = self.settings_map.get(SETTINGS_KEY).await {
			match serde_json::from_slice::<Settings>(&stored) {
				| Ok(stored) => *self.settings.write().expect("locked") = stored,
				| Err(e) => warn!("Moderation settings unreadable, using defaults: {e}"),
			}
		}

		let mut rx = self
			.rx
			.lock()
			.expect("locked")
			.take()
			.expect("worker runs once");

		let mut batch = Vec::new();
		let mut deadline: Option<tokio::time::Instant> = None;
		loop {
			let wait = async {
				match deadline {
					| Some(at) => tokio::time::sleep_until(at).await,
					| None => std::future::pending::<()>().await,
				}
			};

			tokio::select! {
				item = rx.recv() => {
					let Some(item) = item else { return Ok(()) };
					batch.push(item);
					if deadline.is_none() {
						let minutes = self.settings().batch_minutes.clamp(1, 60);
						deadline = Some(tokio::time::Instant::now() + Duration::from_secs(minutes.saturating_mul(60)));
					}
					if batch.len() < BATCH_MAX {
						continue;
					}
				},
				() = wait => {},
				() = self.services.server.until_shutdown() => return Ok(()),
			}

			deadline = None;
			let items = std::mem::take(&mut batch);
			if let Err(e) = self.check_batch(items).await {
				self.services
					.admin
					.notify_loud(&format!("AI moderation: a check failed: {e}"))
					.await;
			}
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// The current settings.
#[implement(Service)]
#[must_use]
pub fn settings(&self) -> Settings { self.settings.read().expect("locked").clone() }

/// Checks and stores new settings. Aliases in `extra` are resolved to room IDs.
#[implement(Service)]
pub async fn set_settings(&self, mut settings: Settings, extra: &[String]) -> Result<Settings> {
	let mut rooms = Vec::new();
	for room in extra.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
		let Ok(room) = <&RoomOrAliasId>::try_from(room) else {
			return Err!(Request(InvalidParam("\"{room}\" isn't a room ID or alias.")));
		};

		let room_id = self
			.services
			.alias
			.maybe_resolve(room)
			.await
			.map_err(|e| err!(Request(NotFound("Couldn't find {room}: {e}"))))?;

		if !rooms.contains(&room_id) {
			rooms.push(room_id);
		}
	}

	settings.extra_rooms = rooms;
	settings.batch_minutes = settings.batch_minutes.clamp(1, 60);
	settings.rules.truncate(4000);
	for model in [&mut settings.text_model, &mut settings.vision_model] {
		let trimmed = model.trim();
		*model = if trimmed.is_empty() { "auto".into() } else { trimmed.into() };
	}

	self.settings_map
		.insert(SETTINGS_KEY, serde_json::to_vec(&settings)?);
	(*self.settings.write().expect("locked")).clone_from(&settings);

	Ok(settings)
}

/// The Venice API key, read fresh so a newly saved one works at once.
#[implement(Service)]
pub fn key(&self) -> Option<String> {
	let path = self.services.config.ai_key_file.as_ref()?;
	let key = std::fs::read_to_string(path).ok()?;
	let key = key.trim();
	(!key.is_empty()).then(|| key.to_owned())
}

#[implement(Service)]
fn client(&self) -> &reqwest::Client { &self.services.client.clients.default }

/// Venice's models, cached for an hour.
#[implement(Service)]
pub async fn models(&self) -> Result<Vec<Model>> {
	if let Some((at, models)) = &*self.models.lock().expect("locked") {
		if at.elapsed() < MODELS_TTL {
			return Ok(models.clone());
		}
	}

	let models = venice::models(self.client()).await?;
	*self.models.lock().expect("locked") = Some((Instant::now(), models.clone()));

	Ok(models)
}

/// The model to use: the configured one, or Venice's pick for "auto".
#[implement(Service)]
async fn model(&self, configured: &str, vision: bool) -> Result<String> {
	if configured != "auto" {
		return Ok(configured.to_owned());
	}

	let wanted = if vision { "default_vision" } else { "default" };
	self.models()
		.await?
		.into_iter()
		.find(|m| m.traits.iter().any(|t| t == wanted))
		.map(|m| m.id)
		.ok_or_else(|| err!("Venice names no {wanted} model; pick one in Settings"))
}

/// Queues a new event if it may need a look. Cheap; the work happens in the
/// worker.
#[implement(Service)]
pub fn notify(&self, pdu: &PduEvent) {
	if *pdu.kind() != TimelineEventType::RoomMessage
		|| pdu.sender() == self.services.globals.server_user
		|| pdu.content().get().len() > MAX_PDU_BYTES
		|| !self.settings.read().expect("locked").enabled
	{
		return;
	}

	let Ok(content) = serde_json::from_str(pdu.content().get()) else {
		return;
	};

	let item = Item {
		event_id: pdu.event_id().to_owned(),
		room_id: pdu.room_id().to_owned(),
		sender: pdu.sender().to_owned(),
		content,
	};

	if self.tx.try_send(item).is_err() {
		debug!("Moderation queue full; skipping an event");
	}
}

/// Whether the settings cover the room. Encrypted rooms are never watched.
#[implement(Service)]
async fn watched(&self, settings: &Settings, room_id: &RoomId) -> bool {
	let state = &self.services.state_accessor;
	if state.is_encrypted_room(room_id).await {
		return false;
	}

	settings.extra_rooms.iter().any(|r| r == room_id)
		|| (settings.watch_directory && self.services.directory.is_public_room(room_id).await)
		|| (settings.watch_public_joined
			&& matches!(state.get_join_rules(room_id).await, JoinRule::Public))
}

/// Placeholders for the people and rooms in what the AI is sent.
#[derive(Default)]
struct Aliases {
	users: HashMap<String, String>,
	rooms: HashMap<String, String>,
}

impl Aliases {
	fn user(&mut self, id: &str) -> String {
		let n = self.users.len().saturating_add(1);
		self.users
			.entry(id.to_owned())
			.or_insert_with(|| format!("U{n}"))
			.clone()
	}

	fn room(&mut self, id: &str) -> String {
		let n = self.rooms.len().saturating_add(1);
		self.rooms
			.entry(id.to_owned())
			.or_insert_with(|| format!("R{n}"))
			.clone()
	}
}

/// Text with every Matrix user ID, room ID and alias replaced by a
/// placeholder.
fn pseudonymize(text: &str) -> String {
	use std::sync::LazyLock;

	static IDS: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(r"[@!#$+][A-Za-z0-9._=/+\-]+:[A-Za-z0-9.\-]+(?::[0-9]+)?").expect("valid regex")
	});

	IDS.replace_all(text, |c: &regex::Captures<'_>| match c[0].as_bytes()[0] {
		| b'@' => "@someone",
		| _ => "#a-room",
	})
	.into_owned()
}

fn excerpt(text: &str) -> String { text.chars().take(EXCERPT).collect() }

/// What a batch has to check.
struct Message {
	item: Item,
	body: String,
}

struct Image {
	item: Item,
	mxc: String,
	mime: String,
}

#[implement(Service)]
async fn check_batch(&self, items: Vec<Item>) -> Result {
	let settings = self.settings();
	if !settings.enabled {
		return Ok(());
	}

	let mut watched: HashMap<OwnedRoomId, bool> = HashMap::new();
	let mut messages = Vec::new();
	let mut images = Vec::new();

	for item in items {
		let room_watched = match watched.get(&item.room_id) {
			| Some(w) => *w,
			| None => {
				let w = self.watched(&settings, &item.room_id).await;
				watched.insert(item.room_id.clone(), w);
				w
			},
		};
		if !room_watched {
			continue;
		}

		let content = &item.content;
		let msgtype = content["msgtype"].as_str().unwrap_or_default();
		let body = content["body"].as_str().unwrap_or_default().to_owned();
		let size = content["info"]["size"].as_u64().unwrap_or(0);
		let mime = content["info"]["mimetype"]
			.as_str()
			.unwrap_or_default()
			.to_owned();
		let url = content["url"].as_str().map(ToOwned::to_owned);

		let large = settings.large_file_mb > 0
			&& size >= settings.large_file_mb.saturating_mul(1 << 20);
		if large && url.is_some() {
			self.large_file(&item, url.clone(), size, &mime, &body)
				.await;
		}

		match (msgtype, url) {
			| ("m.text" | "m.notice" | "m.emote", _) if settings.check_messages && !body.is_empty() =>
				messages.push(Message { item, body }),
			| ("m.image", Some(mxc))
				if settings.check_images
					&& size <= IMAGE_MAX_BYTES
					&& (mime.is_empty() || IMAGE_TYPES.contains(&mime.as_str())) =>
				images.push(Image { item, mxc, mime }),
			| _ => {},
		}
	}

	if messages.is_empty() && images.is_empty() {
		return Ok(());
	}

	let Some(key) = self.key() else {
		return Err!("no Venice API key is saved, so nothing was checked");
	};

	if !messages.is_empty() {
		self.check_messages(&settings, &key, messages).await?;
	}

	for image in images.into_iter().take(IMAGES_PER_BATCH) {
		if let Err(e) = self.check_image(&settings, &key, &image).await {
			debug!("Image check failed: {e}");
		}
	}

	Ok(())
}

#[implement(Service)]
async fn check_messages(&self, settings: &Settings, key: &str, messages: Vec<Message>) -> Result {
	let model = self.model(&settings.text_model, false).await?;
	let mut aliases = Aliases::default();
	let mut list = String::new();
	for (n, m) in messages.iter().enumerate() {
		let user = aliases.user(m.item.sender.as_str());
		let room = aliases.room(m.item.room_id.as_str());
		let text = pseudonymize(&excerpt(&m.body)).replace('\n', " ");
		writeln!(list, "{}. [{user} in {room}] {text}", n.saturating_add(1))?;
	}

	let system = format!(
		"You moderate the public rooms of a Matrix chat server. Its rules:\n{rules}\n\nYou \
		 get numbered messages. People are shown as U1, U2 and rooms as R1, R2. Flag only \
		 messages that clearly break the rules or the law. Swearing, opinions, jokes and \
		 arguments are fine. Reply with only JSON: {{\"flags\":[{{\"n\":<message \
		 number>,\"category\":\"spam|scam|harassment|threat|doxxing|csam|illegal|other\",\"\
		 severity\":\"low|medium|high\",\"reason\":\"<one short sentence>\"}}]}}. Reply \
		 {{\"flags\":[]}} when nothing breaks them.",
		rules = settings.rules
	);

	let reply =
		venice::chat(self.client(), key, &model, &system, &[], vec![Part::Text(list)]).await?;
	let Some(flags) = json_in(&reply).and_then(|v| v["flags"].as_array().cloned()) else {
		return Err!("the AI's answer wasn't the expected JSON");
	};

	for flag in flags {
		let Some(m) = flag["n"]
			.as_u64()
			.and_then(|n| usize::try_from(n).ok())
			.and_then(|n| n.checked_sub(1))
			.and_then(|n| messages.get(n))
		else {
			continue;
		};

		let category = text_field(&flag["category"], "other");
		let severity = severity(&flag["severity"]);
		let mut report = self
			.new_report("message", &m.item, "ai")
			.await;
		report.excerpt = excerpt(&m.body);
		report.suggested = if severity == "low" { "review" } else { "redact" }.into();
		report.category = category;
		report.severity = severity;
		report.reason = text_field(&flag["reason"], "");
		self.save_report(&report);
	}

	Ok(())
}

#[implement(Service)]
async fn check_image(&self, settings: &Settings, key: &str, image: &Image) -> Result {
	let mxc: Mxc<'_> = image.mxc.as_str().try_into()?;
	let media = self.services.media.get_stored(&mxc).await?;
	let mime = media
		.content_type
		.clone()
		.filter(|t| IMAGE_TYPES.contains(&t.as_str()))
		.unwrap_or_else(|| image.mime.clone());
	if !IMAGE_TYPES.contains(&mime.as_str()) {
		return Ok(());
	}

	let model = self.model(&settings.vision_model, true).await?;
	let system = format!(
		"You check images posted in the public rooms of a chat server. Its rules:\n{rules}\n\n\
		 Reply with only JSON: {{\"verdict\":\"ok|questionable|bad\",\"category\":\"<short \
		 word>\",\"reason\":\"<one short sentence>\"}}. Say \"bad\" only when the image clearly \
		 breaks the rules or the law; \"questionable\" when a person should look.",
		rules = settings.rules
	);

	let parts = vec![
		Part::Text("Check this image.".into()),
		Part::Image { mime, data: media.content },
	];
	let reply = venice::chat(self.client(), key, &model, &system, &[], parts).await?;
	let Some(answer) = json_in(&reply) else {
		return Err!("the AI's answer wasn't the expected JSON");
	};

	let verdict = answer["verdict"].as_str().unwrap_or("ok");
	if verdict == "ok" {
		return Ok(());
	}

	let bad = verdict == "bad";
	let mut report = self
		.new_report("image", &image.item, "ai")
		.await;
	report.mxc = Some(image.mxc.clone());
	report.excerpt = excerpt(image.item.content["body"].as_str().unwrap_or_default());
	report.category = text_field(&answer["category"], "other");
	report.severity = if bad { "high" } else { "medium" }.into();
	report.reason = text_field(&answer["reason"], "");
	report.suggested = "hide".into();

	if bad && settings.media_mode == MediaMode::Hide {
		self.services
			.media
			.hide(&image.mxc, &format!("report {}", report.id));
		report.hidden = true;
	}

	self.save_report(&report);
	if report.hidden {
		self.services
			.admin
			.notify_loud(&format!(
				"AI moderation hid an image until you review it: {} ({}). See Reports.",
				report.reason, report.category
			))
			.await;
	}

	Ok(())
}

#[implement(Service)]
async fn large_file(&self, item: &Item, mxc: Option<String>, size: u64, mime: &str, body: &str) {
	let mut report = self.new_report("large_file", item, "size").await;
	report.mxc = mxc;
	report.excerpt = excerpt(body);
	report.category = "large file".into();
	report.severity = "low".into();
	report.reason = format!(
		"{} MiB {}",
		size >> 20,
		if mime.is_empty() { "file" } else { mime }
	);
	report.suggested = "review".into();
	self.save_report(&report);
}

fn text_field(value: &Value, default: &str) -> String {
	value
		.as_str()
		.unwrap_or(default)
		.chars()
		.take(300)
		.collect()
}

fn severity(value: &Value) -> String {
	match value.as_str() {
		| Some(s @ ("low" | "medium" | "high")) => s.into(),
		| _ => "medium".into(),
	}
}

#[implement(Service)]
async fn new_report(&self, kind: &str, item: &Item, source: &str) -> Report {
	let now = now_millis();
	let id = {
		let mut last = self.last_id.lock().expect("locked");
		*last = now.max(last.saturating_add(1));
		*last
	};

	Report {
		id,
		ts: now,
		kind: kind.into(),
		room_id: item.room_id.to_string(),
		room_name: self
			.services
			.state_accessor
			.get_name(&item.room_id)
			.await
			.ok(),
		event_id: item.event_id.to_string(),
		sender: item.sender.to_string(),
		mxc: None,
		excerpt: String::new(),
		category: String::new(),
		severity: "medium".into(),
		reason: String::new(),
		suggested: "review".into(),
		hidden: false,
		status: "open".into(),
		source: source.into(),
	}
}

#[implement(Service)]
fn save_report(&self, report: &Report) {
	if let Ok(value) = serde_json::to_vec(report) {
		self.reports_map
			.insert(&report.id.to_be_bytes(), value);
	}
}

/// Reports, newest first; only open ones unless `all`. Prunes the oldest
/// closed reports past the most kept.
#[implement(Service)]
pub async fn reports(&self, all: bool, limit: usize) -> Vec<Report> {
	let reports: Vec<Report> = self
		.reports_map
		.rev_raw_stream()
		.ignore_err()
		.filter_map(async |(_, value)| serde_json::from_slice(value).ok())
		.collect()
		.await;

	for old in reports
		.iter()
		.skip(REPORTS_KEPT)
		.filter(|r| r.status != "open")
	{
		self.reports_map.remove(&old.id.to_be_bytes());
	}

	reports
		.into_iter()
		.filter(|r| all || r.status == "open")
		.take(limit)
		.collect()
}

#[implement(Service)]
pub async fn report(&self, id: u64) -> Result<Report> {
	let value = self
		.reports_map
		.get(&id.to_be_bytes())
		.await
		.map_err(|_| err!(Request(NotFound("No such report."))))?;

	Ok(serde_json::from_slice(&value)?)
}

/// Applies an admin's decision: "dismiss", "resolve", "reopen", "hide" or
/// "unhide". Redacting is done by the panel as the admin, through the
/// ordinary client API.
#[implement(Service)]
pub async fn act(&self, id: u64, action: &str) -> Result<Report> {
	let mut report = self.report(id).await?;
	match action {
		| "dismiss" => report.status = "dismissed".into(),
		| "resolve" => report.status = "resolved".into(),
		| "reopen" => report.status = "open".into(),
		| "hide" | "unhide" => {
			let Some(mxc) = report.mxc.clone() else {
				return Err!(Request(InvalidParam("This report has no media.")));
			};
			if action == "hide" {
				self.services
					.media
					.hide(&mxc, &format!("report {id}"));
			} else {
				self.services.media.unhide(&mxc);
			}
			report.hidden = action == "hide";
		},
		| _ => return Err!(Request(InvalidParam("Unknown action."))),
	}

	self.save_report(&report);
	Ok(report)
}

/// Checks the key and model choice with a tiny request.
#[implement(Service)]
pub async fn test(&self) -> Result<String> {
	let Some(key) = self.key() else {
		return Err!(Request(NotFound("No Venice API key is saved yet.")));
	};

	let settings = self.settings();
	let model = self.model(&settings.text_model, false).await?;
	let vision = self.model(&settings.vision_model, true).await?;
	let reply = venice::chat(
		self.client(),
		&key,
		&model,
		"Answer with the single word: ready",
		&[],
		vec![Part::Text("Are you there?".into())],
	)
	.await?;

	Ok(format!(
		"Venice answered \"{}\" using {model}. Images go to {vision}.",
		reply.chars().take(40).collect::<String>()
	))
}

/// The panel's assistant: answers the admin, and may propose admin commands
/// in ```command blocks, which run only when the admin clicks them.
#[implement(Service)]
pub async fn assistant(&self, history: &[Turn]) -> Result<String> {
	let Some(key) = self.key() else {
		return Err!(Request(NotFound("No Venice API key is saved yet.")));
	};

	let settings = self.settings();
	let model = self.model(&settings.text_model, false).await?;
	let open = self.reports(false, 20).await;
	let mut context = String::new();
	for r in &open {
		writeln!(
			context,
			"- report {}: {} {} ({}, {}): {}",
			r.id,
			r.kind,
			r.category,
			r.severity,
			r.suggested,
			pseudonymize(&r.reason)
		)?;
	}

	let system = format!(
		"You help the admin of a small private Matrix homeserver (tuwunel-based) run it from an \
		 admin panel. Be brief and plain. When an admin command would help, put each one alone \
		 in a fenced block marked command, without the !admin prefix, for example:\n```command\n\
		 users list-users\n```\nThe admin reviews and runs it; never claim you ran anything. \
		 Command groups: users, rooms, media, federation, server, appservices, debug, query, \
		 check; \"<group> --help\" lists a group's commands. Never suggest commands that delete \
		 data without saying so plainly. Open moderation reports (people and rooms are \
		 hidden):\n{context}"
	);

	let history: Vec<Turn> = history
		.iter()
		.rev()
		.take(20)
		.rev()
		.map(|t| Turn {
			role: t.role.clone(),
			content: t.content.chars().take(8000).collect(),
		})
		.collect();

	venice::chat(self.client(), &key, &model, &system, &history, Vec::new()).await
}

#[cfg(test)]
mod tests {
	use super::pseudonymize;

	#[test]
	fn ids_never_reach_the_ai() {
		assert_eq!(
			pseudonymize("ask @nully:anoncat.gleeze.com in #help:matrix.org or !abc:x.org:8448"),
			"ask @someone in #a-room or #a-room"
		);
		assert_eq!(pseudonymize("email me@home or 10:30"), "email me@home or 10:30");
	}
}
