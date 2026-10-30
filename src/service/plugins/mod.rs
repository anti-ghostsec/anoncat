//! WebAssembly plugins.
//!
//! A plugin is a WebAssembly component in `<database_path>/plugins/<name>.wasm`
//! with a manifest beside it, `<name>.toml`, declaring what it may do. Plugins
//! are loaded, unloaded and reloaded while the server runs (`!admin plugins`).
//! Each runs in its own sandbox: WASI with no files, network or environment,
//! a memory cap, and a time limit on every call.
//!
//! Events reach plugins through a queue, so a slow or broken plugin never
//! holds up the server; only `check-event`, which can refuse a local user's
//! message, runs inline, under a short time limit and failing open.

use std::{
	collections::BTreeMap,
	path::PathBuf,
	sync::{Arc, Mutex, RwLock},
	time::Duration,
};

use async_trait::async_trait;
use ruma::{OwnedRoomId, RoomId, events::room::message::RoomMessageEventContent};
use serde::Deserialize;
use tokio::sync::mpsc;
use tuwunel_core::{Err, Result, debug, err, error, implement, info, toml, warn};
use tuwunel_database::Map;
use tuwunel_matrix::{Event, PduEvent};
use wasmtime::{
	Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
	component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
	wasmtime::component::bindgen!({
		path: "plugins/wit",
		world: "plugin",
	});
}

use self::bindings::{
	Plugin,
	anoncat::plugin::{host, types},
};
pub use self::bindings::anoncat::plugin::types::{Event as PluginEvent, Verdict};

/// How often the engine's epoch advances; call time limits count in these.
const TICK: Duration = Duration::from_millis(10);

/// Time limits per call, in ticks.
const INIT_TICKS: u64 = 500;
const CHECK_TICKS: u64 = 20;
const EVENT_TICKS: u64 = 100;
const COMMAND_TICKS: u64 = 1000;

const MEMORY_LIMIT: usize = 64 << 20;
const EVENT_QUEUE: usize = 1024;
const SEND_QUEUE: usize = 256;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	engine: Engine,
	linker: Linker<State>,
	kv: Arc<Map>,
	plugins: RwLock<BTreeMap<String, Arc<Loaded>>>,
	events: mpsc::Sender<Arc<PluginEvent>>,
	events_rx: Mutex<Option<mpsc::Receiver<Arc<PluginEvent>>>>,
	sends: mpsc::Sender<Outgoing>,
	sends_rx: Mutex<Option<mpsc::Receiver<Outgoing>>>,
}

/// A plugin's manifest, `<name>.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
	/// Load when the server starts.
	pub enabled: bool,
	pub description: String,
	/// May refuse local users' messages (`check-event`).
	pub check_events: bool,
	/// May send messages as the server's bot account.
	pub send_messages: bool,
	/// Event types delivered to `on-event`; "*" for all, empty for none.
	pub events: Vec<String>,
	/// Handed to the plugin's `init` as JSON.
	pub config: toml::Table,
}

/// A loaded plugin.
pub struct Loaded {
	pub name: String,
	pub manifest: Manifest,
	instance: Mutex<Instance>,
}

struct Instance {
	store: Store<State>,
	bindings: Plugin,
}

/// What a plugin's calls into the host can reach.
struct State {
	name: String,
	send_messages: bool,
	kv: Arc<Map>,
	sends: mpsc::Sender<Outgoing>,
	runtime: tokio::runtime::Handle,
	wasi: WasiCtx,
	table: ResourceTable,
	limits: StoreLimits,
}

/// A message a plugin asked to send.
struct Outgoing {
	plugin: String,
	room_id: OwnedRoomId,
	body: String,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let mut config = Config::new();
		config.epoch_interruption(true);
		let engine = Engine::new(&config).map_err(|e| err!("Plugin engine: {e}"))?;

		let mut linker = Linker::new(&engine);
		wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
			.map_err(|e| err!("Plugin WASI linker: {e}"))?;
		Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
			.map_err(|e| err!("Plugin linker: {e}"))?;

		// Advances the clock the call time limits are measured against.
		let ticker = engine.weak();
		std::thread::Builder::new()
			.name("plugin-epoch".into())
			.spawn(move || {
				while let Some(engine) = ticker.upgrade() {
					engine.increment_epoch();
					drop(engine);
					std::thread::sleep(TICK);
				}
			})?;

		let (events, events_rx) = mpsc::channel(EVENT_QUEUE);
		let (sends, sends_rx) = mpsc::channel(SEND_QUEUE);

		Ok(Arc::new(Self {
			services: args.services.clone(),
			engine,
			linker,
			kv: args.db["pluginid_kv"].clone(),
			plugins: RwLock::default(),
			events,
			events_rx: Mutex::new(Some(events_rx)),
			sends,
			sends_rx: Mutex::new(Some(sends_rx)),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.load_enabled().await;

		let mut events = self.events_rx.lock().expect("locked").take().expect("worker runs once");
		let mut sends = self.sends_rx.lock().expect("locked").take().expect("worker runs once");

		loop {
			tokio::select! {
				Some(event) = events.recv() => self.deliver(event).await,
				Some(send) = sends.recv() => self.send(send).await,
				() = self.services.server.until_shutdown() => return Ok(()),
			}
		}
	}

	async fn clear_cache(&self) {}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// The plugins directory, `<database_path>/plugins`.
#[implement(Service)]
#[must_use]
pub fn dir(&self) -> PathBuf { self.services.config.database_path.join("plugins") }

/// The plugins currently loaded.
#[implement(Service)]
pub fn loaded(&self) -> Vec<Arc<Loaded>> {
	self.plugins
		.read()
		.expect("locked")
		.values()
		.cloned()
		.collect()
}

/// The plugins in the plugins directory, loaded or not, with their manifests.
#[implement(Service)]
pub fn available(&self) -> Result<Vec<(String, Result<Manifest>)>> {
	let dir = self.dir();
	if !dir.exists() {
		return Ok(Vec::new());
	}

	let mut found = Vec::new();
	for entry in std::fs::read_dir(&dir)? {
		let path = entry?.path();
		if path.extension().is_some_and(|ext| ext == "wasm")
			&& let Some(name) = path.file_stem().and_then(|s| s.to_str())
		{
			found.push((name.to_owned(), self.manifest(name)));
		}
	}

	found.sort_by(|a, b| a.0.cmp(&b.0));
	Ok(found)
}

#[implement(Service)]
fn manifest(&self, name: &str) -> Result<Manifest> {
	let path = self.dir().join(format!("{name}.toml"));
	let text = std::fs::read_to_string(&path)
		.map_err(|e| err!("No manifest at {}: {e}", path.display()))?;

	toml::from_str(&text).map_err(|e| err!("Bad manifest {}: {e}", path.display()))
}

#[implement(Service)]
async fn load_enabled(self: &Arc<Self>) {
	let Ok(available) = self.available() else {
		return;
	};

	for (name, manifest) in available {
		if manifest.is_ok_and(|m| m.enabled) {
			match self.load(&name).await {
				| Ok(()) => info!(plugin = %name, "Loaded plugin"),
				| Err(e) => error!(plugin = %name, "Failed to load plugin: {e}"),
			}
		}
	}
}

/// Loads (or reloads) a plugin from the plugins directory and calls its
/// `init`. A plugin of the same name already loaded is replaced only once
/// the new one has started.
#[implement(Service)]
pub async fn load(self: &Arc<Self>, name: &str) -> Result {
	if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
		return Err!(Request(InvalidParam("Plugin names are letters, digits, - and _.")));
	}

	let manifest = self.manifest(name)?;
	let path = self.dir().join(format!("{name}.wasm"));
	let this = self.clone();
	let name = name.to_owned();

	let loaded = tokio::task::spawn_blocking(move || this.instantiate(name, manifest, &path))
		.await
		.map_err(|e| err!("Plugin load task: {e}"))??;

	let loaded = Arc::new(loaded);
	let config = serde_json::to_string(&loaded.manifest.config)?;
	let init = self
		.call(&loaded, INIT_TICKS, move |b, s| b.call_init(s, &config))
		.await?;

	if let Err(reason) = init {
		return Err!("Plugin {} refused to start: {reason}", loaded.name);
	}

	self.plugins
		.write()
		.expect("locked")
		.insert(loaded.name.clone(), loaded);

	Ok(())
}

#[implement(Service)]
fn instantiate(&self, name: String, manifest: Manifest, path: &std::path::Path) -> Result<Loaded> {
	let component = Component::from_file(&self.engine, path)
		.map_err(|e| err!("Can't compile {}: {e}", path.display()))?;

	let state = State {
		name: name.clone(),
		send_messages: manifest.send_messages,
		kv: self.kv.clone(),
		sends: self.sends.clone(),
		runtime: tokio::runtime::Handle::current(),
		// Nothing: no files, environment, arguments, network or stdio.
		wasi: WasiCtxBuilder::new().build(),
		table: ResourceTable::new(),
		limits: StoreLimitsBuilder::new()
			.memory_size(MEMORY_LIMIT)
			.instances(8)
			.build(),
	};

	let mut store = Store::new(&self.engine, state);
	store.limiter(|state| &mut state.limits);
	store.set_epoch_deadline(INIT_TICKS);

	let bindings = Plugin::instantiate(&mut store, &component, &self.linker)
		.map_err(|e| err!("Can't start {}: {e}", path.display()))?;

	Ok(Loaded {
		name,
		manifest,
		instance: Mutex::new(Instance { store, bindings }),
	})
}

/// Unloads a plugin. Calls already running finish first.
#[implement(Service)]
pub fn unload(&self, name: &str) -> Result {
	self.plugins
		.write()
		.expect("locked")
		.remove(name)
		.map(|_| ())
		.ok_or_else(|| err!(Request(NotFound("No plugin {name} is loaded."))))
}

/// Calls a plugin's `on-command`.
#[implement(Service)]
pub async fn command(&self, name: &str, args: Vec<String>) -> Result<String> {
	let Some(plugin) = self.plugins.read().expect("locked").get(name).cloned() else {
		return Err!(Request(NotFound("No plugin {name} is loaded.")));
	};

	self.call(&plugin, COMMAND_TICKS, move |b, s| b.call_on_command(s, &args))
		.await
}

/// Asks the plugins allowed to check events whether a local user may send
/// this. The first refusal wins; a plugin that fails or times out allows it.
#[implement(Service)]
pub async fn check(&self, event: PluginEvent) -> Result<(), String> {
	let checkers: Vec<_> = self
		.plugins
		.read()
		.expect("locked")
		.values()
		.filter(|p| p.manifest.check_events)
		.cloned()
		.collect();

	if checkers.is_empty() {
		return Ok(());
	}

	let event = Arc::new(event);
	for plugin in checkers {
		let event = event.clone();
		match self
			.call(&plugin, CHECK_TICKS, move |b, s| b.call_check_event(s, &event))
			.await
		{
			| Ok(Verdict::Allow) => {},
			| Ok(Verdict::Reject(reason)) => return Err(reason),
			| Err(e) => debug!(plugin = %plugin.name, "check-event failed, allowing: {e}"),
		}
	}

	Ok(())
}

/// Queues a stored event for the plugins subscribed to its type. Never waits:
/// when the queue is full the event is dropped for plugins.
#[implement(Service)]
pub fn notify(&self, pdu: &PduEvent) {
	let kind = pdu.kind().to_string();
	let wanted = self
		.plugins
		.read()
		.expect("locked")
		.values()
		.any(|p| subscribed(&p.manifest, &kind));

	if !wanted {
		return;
	}

	let event = PluginEvent {
		event_id: pdu.event_id().to_string(),
		room_id: pdu.room_id().to_string(),
		sender: pdu.sender().to_string(),
		kind,
		state_key: pdu.state_key().map(ToOwned::to_owned),
		content: pdu.content().get().to_owned(),
		origin_server_ts: pdu.origin_server_ts().get().into(),
	};

	if self.events.try_send(Arc::new(event)).is_err() {
		debug!("Plugin event queue full; dropping an event");
	}
}

fn subscribed(manifest: &Manifest, kind: &str) -> bool {
	manifest.events.iter().any(|k| k == "*" || k == kind)
}

#[implement(Service)]
async fn deliver(&self, event: Arc<PluginEvent>) {
	let plugins: Vec<_> = self
		.plugins
		.read()
		.expect("locked")
		.values()
		.filter(|p| subscribed(&p.manifest, &event.kind))
		.cloned()
		.collect();

	for plugin in plugins {
		let event = event.clone();
		if let Err(e) = self
			.call(&plugin, EVENT_TICKS, move |b, s| b.call_on_event(s, &event))
			.await
		{
			debug!(plugin = %plugin.name, "on-event failed: {e}");
		}
	}
}

#[implement(Service)]
async fn send(&self, send: Outgoing) {
	let content = RoomMessageEventContent::text_plain(send.body);
	if let Err(e) = self
		.services
		.admin
		.send_to_room(content, &send.room_id)
		.await
	{
		warn!(plugin = %send.plugin, room_id = %send.room_id, "Plugin message not sent: {e}");
	}
}

/// Runs one call into a plugin on a blocking thread, under a time limit.
///
/// A call that fails (a trap: timeout, memory limit, panic) leaves the
/// instance unusable, so the plugin is unloaded at once and the admin room
/// is told; `!admin plugins load` starts it fresh.
#[implement(Service)]
async fn call<R, F>(&self, plugin: &Arc<Loaded>, ticks: u64, f: F) -> Result<R>
where
	R: Send + 'static,
	F: FnOnce(&Plugin, &mut Store<State>) -> wasmtime::Result<R> + Send + 'static,
{
	let target = plugin.clone();
	let result = tokio::task::spawn_blocking(move || {
		let mut instance = target.instance.lock().expect("locked");
		let Instance { store, bindings } = &mut *instance;
		store.set_epoch_deadline(ticks);
		f(bindings, store)
	})
	.await
	.map_err(|e| err!("Plugin call task: {e}"))?;

	match result {
		| Ok(value) => Ok(value),
		| Err(e) => {
			// Only the instance that failed: a reload may already have replaced it.
			let current = self
				.plugins
				.read()
				.expect("locked")
				.get(&plugin.name)
				.is_some_and(|loaded| Arc::ptr_eq(loaded, plugin));

			if current && self.unload(&plugin.name).is_ok() {
				let notice = format!("Plugin {} failed and was unloaded: {e:#}", plugin.name);
				error!("{notice}");
				self.services.admin.notify_loud(&notice).await;
			}

			Err!("Plugin {} failed: {e:#}", plugin.name)
		},
	}
}

// --- host functions ------------------------------------------------------------

impl WasiView for State {
	fn ctx(&mut self) -> WasiCtxView<'_> {
		WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
	}
}

impl types::Host for State {}

impl host::Host for State {
	fn log(&mut self, level: host::Level, message: String) {
		let plugin = &self.name;
		match level {
			| host::Level::Error => error!(%plugin, "{message}"),
			| host::Level::Warn => warn!(%plugin, "{message}"),
			| host::Level::Info => info!(%plugin, "{message}"),
			| host::Level::Debug => debug!(%plugin, "{message}"),
		}
	}

	fn kv_get(&mut self, key: String) -> Option<Vec<u8>> {
		let key = kv_key(&self.name, &key);
		self.runtime
			.block_on(self.kv.get(&key))
			.ok()
			.map(|value| value.to_vec())
	}

	fn kv_set(&mut self, key: String, value: Vec<u8>) {
		self.kv.insert(&kv_key(&self.name, &key), &value);
	}

	fn kv_delete(&mut self, key: String) { self.kv.remove(&kv_key(&self.name, &key)); }

	fn send_message(&mut self, room_id: String, body: String) -> Result<(), String> {
		if !self.send_messages {
			return Err("this plugin lacks the send_messages capability".into());
		}

		let room_id = RoomId::parse(&room_id)
			.map_err(|e| format!("bad room ID: {e}"))?
			.to_owned();

		self.sends
			.try_send(Outgoing { plugin: self.name.clone(), room_id, body })
			.map_err(|_| "too many messages queued".to_owned())
	}
}

/// Each plugin's keys live under its own name.
fn kv_key(plugin: &str, key: &str) -> Vec<u8> {
	let mut out = Vec::with_capacity(plugin.len() + 1 + key.len());
	out.extend_from_slice(plugin.as_bytes());
	out.push(0xFF);
	out.extend_from_slice(key.as_bytes());
	out
}
