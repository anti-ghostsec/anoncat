// anoncat admin panel. Served only on the loopback admin listener; talks to
// the server's own admin APIs on the same origin with the signed-in admin's
// access token, kept in sessionStorage (gone when the tab closes).
//
// Everything from the server is inserted as text, never as HTML.
"use strict";

const TOKEN_KEY = "anoncat.token";
const DEVICE_KEY = "anoncat.device";
const PANEL_DEVICE = "anoncat admin panel";
const USER_KEY = "anoncat.user";
const PAGE_SIZE = 50;

const $ = (sel, root = document) => root.querySelector(sel);

// --- DOM -------------------------------------------------------------------

function h(tag, attrs = {}, ...children) {
	const el = document.createElement(tag);
	for (const [key, value] of Object.entries(attrs)) {
		if (value === undefined || value === null || value === false) continue;
		if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
		else if (key === "class") el.className = value;
		else el.setAttribute(key, value === true ? "" : value);
	}
	for (const child of children.flat()) {
		if (child === undefined || child === null || child === false) continue;
		el.append(child instanceof Node ? child : String(child));
	}
	return el;
}

function toast(text, ms = 3500) {
	const el = $("#toast");
	el.textContent = text;
	el.hidden = false;
	clearTimeout(toast.timer);
	toast.timer = setTimeout(() => (el.hidden = true), ms);
}

// A modal with the given fields; resolves to the entered values, or null.
function ask(title, { text, fields = [], confirm = "OK", danger = false } = {}) {
	const dialog = $("#dialog");
	const body = $("#dialog-body");
	const inputs = {};
	body.replaceChildren(
		h("h2", {}, title),
		text ? h("p", {}, text) : null,
		...fields.map((f) => {
			const input = f.type === "checkbox"
				? h("input", { type: "checkbox", name: f.name, checked: f.value })
				: h("input", { name: f.name, type: f.type || "text", value: f.value ?? "", placeholder: f.placeholder, required: f.required });
			inputs[f.name] = input;
			return h("label", {}, f.label, input);
		}),
		h("div", { class: "buttons" },
			h("button", { value: "cancel", class: "ghost", formnovalidate: true }, "Cancel"),
			h("button", { value: "ok", class: danger ? "danger" : "" }, confirm)),
	);
	return new Promise((resolve) => {
		dialog.onclose = () => {
			if (dialog.returnValue !== "ok") return resolve(null);
			const values = {};
			for (const [name, input] of Object.entries(inputs)) values[name] = input.type === "checkbox" ? input.checked : input.value;
			resolve(values);
		};
		dialog.returnValue = "";
		dialog.showModal();
	});
}

// --- API -------------------------------------------------------------------

class ApiError extends Error {
	constructor(status, body) {
		super(body?.error || `HTTP ${status}`);
		this.status = status;
		this.errcode = body?.errcode;
	}
}

async function api(method, path, body) {
	const headers = { Accept: "application/json" };
	const token = sessionStorage.getItem(TOKEN_KEY);
	if (token) headers.Authorization = `Bearer ${token}`;
	if (body !== undefined) headers["Content-Type"] = "application/json";
	const res = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: "omit", cache: "no-store" });
	const text = await res.text();
	let json = null;
	try { json = text ? JSON.parse(text) : {}; } catch { json = { error: text }; }
	if (res.status === 401 && token) {
		signOut(false);
		throw new ApiError(res.status, json);
	}
	if (!res.ok) throw new ApiError(res.status, json);
	return json;
}

const enc = encodeURIComponent;
const command = (cmd) => api("POST", "/_anoncat/admin/v1/command", { command: cmd });

// Runs an action, reporting failure instead of throwing.
async function attempt(what, fn) {
	try {
		const result = await fn();
		if (what) toast(what);
		return result;
	} catch (e) {
		toast(`Failed: ${e.message}`, 6000);
		return undefined;
	}
}

// --- formatting ----------------------------------------------------------------

function bytes(n) {
	if (n === null || n === undefined) return "—";
	const units = ["B", "KiB", "MiB", "GiB", "TiB"];
	let i = 0;
	while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
	return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
}

function duration(secs) {
	if (!secs) return "off";
	const d = Math.floor(secs / 86400), hr = Math.floor((secs % 86400) / 3600), m = Math.floor((secs % 3600) / 60);
	return [d && `${d}d`, hr && `${hr}h`, m && `${m}m`].filter(Boolean).join(" ") || `${secs}s`;
}

function date(ms) {
	return ms ? new Date(ms).toLocaleString() : "—";
}

function meter(fraction) {
	const pct = Math.max(0, Math.min(100, fraction * 100));
	// Set through the DOM: the panel's CSP refuses inline style attributes.
	const fill = h("div");
	fill.style.width = `${pct.toFixed(1)}%`;
	return h("div", { class: `meter ${pct > 90 ? "bad" : pct > 75 ? "warn" : ""}` }, fill);
}

function stat(label, value, sub, extra) {
	return h("div", { class: "card stat" }, h("div", { class: "label" }, label), h("div", { class: "value" }, value), sub ? h("div", { class: "sub" }, sub) : null, extra);
}

function table(columns, rows, empty = "Nothing here.") {
	if (!rows.length) return h("p", { class: "muted" }, empty);
	return h("table", {},
		h("thead", {}, h("tr", {}, columns.map((c) => h("th", {}, c)))),
		h("tbody", {}, rows));
}

function pager(from, total, onPage) {
	const next = from + PAGE_SIZE;
	return h("div", { class: "toolbar" },
		h("button", { class: "ghost small", disabled: from === 0, onclick: () => onPage(Math.max(0, from - PAGE_SIZE)) }, "Previous"),
		h("span", { class: "muted small" }, total ? `${from + 1}–${Math.min(next, total)} of ${total}` : ""),
		h("button", { class: "ghost small", disabled: total !== undefined && next >= total, onclick: () => onPage(next) }, "Next"));
}

// --- pages -----------------------------------------------------------------

const pages = {};

pages.dashboard = async (main) => {
	const s = await api("GET", "/_anoncat/admin/v1/status");
	$("#server-name").textContent = s.server_name;
	const diskUsed = s.disk_total && s.disk_available !== null ? s.disk_total - s.disk_available : null;
	const ramUsed = s.ram_total && s.ram_available !== null ? s.ram_total - s.ram_available : null;
	const c = s.cleanup;
	const output = h("pre", { class: "output", hidden: true });
	const run = (label, cmd) => h("button", { class: "ghost", onclick: async (e) => {
		e.target.disabled = true;
		const r = await attempt(null, () => command(cmd));
		e.target.disabled = false;
		if (r) { output.hidden = false; output.textContent = r.output || (r.ok ? "Done." : "Failed."); }
	} }, label);

	main.replaceChildren(
		h("h1", {}, "Dashboard"),
		h("div", { class: "grid" },
			stat("Disk (vault)", diskUsed !== null ? `${((diskUsed / s.disk_total) * 100).toFixed(0)}%` : "—", `${bytes(diskUsed)} of ${bytes(s.disk_total)}`, s.disk_total ? meter(diskUsed / s.disk_total) : null),
			stat("System memory", ramUsed !== null ? `${((ramUsed / s.ram_total) * 100).toFixed(0)}%` : "—", `${bytes(s.ram_available)} available`, s.ram_total ? meter(ramUsed / s.ram_total) : null),
			stat("Server memory", bytes(s.rss), "resident"),
			stat("Uptime", duration(s.uptime_secs), s.version)),
		h("h2", {}, "Cleanup"),
		table(["Rule", "Setting"], [
			["Runs every", duration(c.interval_secs)],
			["Remote media kept for", duration(c.remote_media_max_age_secs)],
			["Local uploads kept for", duration(c.local_media_max_age_secs)],
			["Local uploads size cap", c.local_media_max_size ? bytes(c.local_media_max_size) : "off"],
			["Message history kept for", duration(c.message_max_age_secs)],
			["Disk guard at", c.disk_percent ? `${c.disk_percent}%` : "off"],
			["Low-memory guard below", c.min_free_ram ? bytes(c.min_free_ram) : "off"],
		].map(([k, v]) => h("tr", {}, h("td", {}, k), h("td", {}, v)))),
		h("p", { class: "muted small" }, "Change these with the cleanup_* options in the config file."),
		h("div", { class: "toolbar" },
			run("Run cleanup now", "server cleanup"),
			run("Clear caches", "server clear-caches"),
			run("Memory details", "server memory-usage"),
			run("Database backups", "server list-backups")),
		output);
};

pages.users = async (main, params) => {
	const from = Number(params.get("from") || 0);
	const q = params.get("q") || "";
	const showDeactivated = params.get("deactivated") === "1";
	const query = new URLSearchParams({ from, limit: PAGE_SIZE, guests: "true", deactivated: showDeactivated ? "true" : "false" });
	if (q) query.set("name", q);
	const res = await api("GET", `/_synapse/admin/v2/users?${query}`);
	const go = (changes) => navigate("users", { from, q, deactivated: showDeactivated ? "1" : "", ...changes });

	// The list leaves out suspension; the per-user details have it.
	await Promise.all(res.users.filter((u) => !u.deactivated).map(async (u) => {
		const details = await api("GET", `/_synapse/admin/v2/users/${enc(u.name)}`).catch(() => null);
		u.suspended = details?.suspended ?? false;
	}));

	const search = h("input", { placeholder: "Search users", value: q, onkeydown: (e) => e.key === "Enter" && go({ q: e.target.value, from: 0 }) });
	const rows = res.users.map((u) => h("tr", {},
		h("td", {}, h("div", {}, u.displayname || u.name), h("div", { class: "mono muted" }, u.name)),
		h("td", {},
			u.admin ? h("span", { class: "tag ok" }, "admin") : null,
			u.deactivated ? h("span", { class: "tag bad" }, "deactivated") : null,
			u.locked ? h("span", { class: "tag bad" }, "locked") : null,
			u.suspended ? h("span", { class: "tag warn" }, "suspended") : null,
			u.is_guest ? h("span", { class: "tag" }, "guest") : null),
		h("td", { class: "small muted" }, date(u.last_seen_ts)),
		h("td", { class: "actions" }, userActions(u, () => go({})))));

	main.replaceChildren(
		h("h1", {}, "Users"),
		h("div", { class: "toolbar" }, search,
			h("label", { class: "small" }, h("input", { type: "checkbox", checked: showDeactivated, onchange: (e) => go({ deactivated: e.target.checked ? "1" : "", from: 0 }) }), " include deactivated")),
		table(["User", "Status", "Last seen", ""], rows, "No users match."),
		pager(from, res.total, (f) => go({ from: f })));
};

function userActions(u, refresh) {
	if (u.deactivated) return [];
	const id = enc(u.name);
	return [
		h("button", { class: "ghost small", onclick: () => userDetails(u.name) }, "Details"),
		h("button", { class: "ghost small", onclick: async () => {
			const suspend = !u.suspended;
			if (await attempt(suspend ? "Suspended" : "Unsuspended", () => api("PUT", `/_synapse/admin/v1/suspend/${id}`, { suspend }))) refresh();
		} }, u.suspended ? "Unsuspend" : "Suspend"),
		h("button", { class: "ghost small", onclick: async () => {
			const locked = !u.locked;
			if (await attempt(locked ? "Locked" : "Unlocked", () => api("PUT", `/_synapse/admin/v2/users/${id}`, { locked }))) refresh();
		} }, u.locked ? "Unlock" : "Lock"),
		h("button", { class: "ghost small", onclick: async () => {
			const v = await ask(`Reset password for ${u.name}`, { fields: [
				{ name: "password", label: "New password", type: "password", required: true },
				{ name: "logout", label: "Sign out all their devices", type: "checkbox", value: true }] , confirm: "Reset" });
			if (v) await attempt("Password reset", () => api("POST", `/_synapse/admin/v1/reset_password/${id}`, { new_password: v.password, logout_devices: v.logout }));
		} }, "Password"),
		h("button", { class: "danger small", onclick: async () => {
			const v = await ask(`Deactivate ${u.name}?`, { text: "They can't sign in again and the user ID can't be reused. This can't be undone.",
				fields: [{ name: "erase", label: "Also erase their profile and hide their messages from new members", type: "checkbox", value: true },
					{ name: "media", label: "Also delete all media they uploaded", type: "checkbox", value: true }],
				confirm: "Deactivate", danger: true });
			if (!v) return;
			if (v.media) await attempt(null, () => api("DELETE", `/_synapse/admin/v1/users/${id}/media`));
			if (await attempt("Deactivated", () => api("POST", `/_synapse/admin/v1/deactivate/${id}`, { erase: v.erase }))) refresh();
		} }, "Deactivate"),
	];
}

async function userDetails(userId) {
	const id = enc(userId);
	const [rooms, media] = await Promise.all([
		attempt(null, () => api("GET", `/_synapse/admin/v1/users/${id}/joined_rooms`)),
		attempt(null, () => api("GET", `/_synapse/admin/v1/users/${id}/media?limit=1`)),
	]);
	await ask(userId, { text: `${rooms?.total ?? "?"} rooms joined · ${media?.total ?? "?"} uploads`, confirm: "Close" });
}

pages.tokens = async (main) => {
	const res = await api("GET", "/_synapse/admin/v1/registration_tokens");
	const refresh = () => render();
	const rows = res.registration_tokens.map((t) => h("tr", {},
		h("td", { class: "mono" }, t.token),
		h("td", {}, `${t.completed} used${t.uses_allowed !== null ? ` of ${t.uses_allowed}` : ""}`, t.pending ? h("span", { class: "muted small" }, ` (${t.pending} pending)`) : null),
		h("td", { class: "small" }, t.expiry_time ? date(t.expiry_time) : "never"),
		h("td", { class: "actions" },
			h("button", { class: "ghost small", onclick: () => navigator.clipboard?.writeText(t.token).then(() => toast("Copied")) }, "Copy"),
			h("button", { class: "danger small", onclick: async () => {
				if (await attempt("Deleted", () => api("DELETE", `/_synapse/admin/v1/registration_tokens/${enc(t.token)}`))) refresh();
			} }, "Delete"))));

	main.replaceChildren(
		h("h1", {}, "Invites"),
		h("p", { class: "muted" }, "Registration tokens. New accounts need one when registration requires a token."),
		h("div", { class: "toolbar" }, h("button", { onclick: async () => {
			const v = await ask("New invite token", { fields: [
				{ name: "uses", label: "Uses allowed (empty = unlimited)", type: "number", value: "1" },
				{ name: "days", label: "Expires after days (empty = never)", type: "number", value: "7" }], confirm: "Create" });
			if (!v) return;
			const body = {};
			if (v.uses) body.uses_allowed = Number(v.uses);
			if (v.days) body.expiry_time = Date.now() + Number(v.days) * 86400000;
			if (await attempt("Created", () => api("POST", "/_synapse/admin/v1/registration_tokens/new", body))) refresh();
		} }, "New token")),
		table(["Token", "Uses", "Expires", ""], rows, "No tokens."));
};

pages.rooms = async (main, params) => {
	const from = Number(params.get("from") || 0);
	const q = params.get("q") || "";
	const query = new URLSearchParams({ from, limit: PAGE_SIZE, order_by: "joined_members", dir: "b" });
	if (q) query.set("search_term", q);
	const res = await api("GET", `/_synapse/admin/v1/rooms?${query}`);
	const go = (changes) => navigate("rooms", { from, q, ...changes });

	const rows = res.rooms.map((r) => h("tr", {},
		h("td", {}, h("div", {}, r.name || r.canonical_alias || "(unnamed)"), h("div", { class: "mono muted" }, r.room_id)),
		h("td", {}, `${r.joined_members}`, h("span", { class: "muted small" }, ` (${r.joined_local_members} here)`)),
		h("td", {},
			r.public ? h("span", { class: "tag" }, "public") : null,
			r.encryption ? h("span", { class: "tag ok" }, "encrypted") : h("span", { class: "tag warn" }, "unencrypted"),
			r.federatable === false ? h("span", { class: "tag" }, "local only") : null),
		h("td", { class: "actions" }, roomActions(r, () => go({})))));

	main.replaceChildren(
		h("h1", {}, "Rooms"),
		h("div", { class: "toolbar" }, h("input", { placeholder: "Search rooms", value: q, onkeydown: (e) => e.key === "Enter" && go({ q: e.target.value, from: 0 }) })),
		table(["Room", "Members", "", ""], rows, "No rooms match."),
		pager(from, res.total_rooms, (f) => go({ from: f })));
};

function roomActions(r, refresh) {
	const id = enc(r.room_id);
	return [
		h("button", { class: "ghost small", onclick: () => navigate("media", { room: r.room_id }) }, "Media"),
		h("button", { class: "ghost small", onclick: async () => {
			const v = await ask(`Delete history in ${r.name || r.room_id}`, {
				text: "Deletes this server's copy of messages before the date. Room state is kept; other servers keep their copies.",
				fields: [{ name: "days", label: "Older than days", type: "number", value: "30", required: true }], confirm: "Delete history", danger: true });
			if (!v) return;
			const ts = Date.now() - Number(v.days) * 86400000;
			await attempt("History purge started", () => api("POST", `/_synapse/admin/v1/purge_history/${id}`, { purge_up_to_ts: ts, delete_local_events: true }));
		} }, "Purge history"),
		h("button", { class: "ghost small", onclick: async () => {
			const block = !(await attempt(null, () => api("GET", `/_synapse/admin/v1/rooms/${id}/block`)))?.block;
			if (await attempt(block ? "Blocked" : "Unblocked", () => api("PUT", `/_synapse/admin/v1/rooms/${id}/block`, { block }))) refresh();
		} }, "Block/unblock"),
		h("button", { class: "danger small", onclick: async () => {
			const v = await ask(`Delete ${r.name || r.room_id}?`, {
				text: "Removes every local user from the room and deletes it from this server.",
				fields: [{ name: "block", label: "Block it so nobody here can rejoin", type: "checkbox", value: true }], confirm: "Delete room", danger: true });
			if (!v) return;
			if (await attempt("Room deletion started", () => api("DELETE", `/_synapse/admin/v2/rooms/${id}`, { block: v.block, purge: true }))) setTimeout(refresh, 1500);
		} }, "Delete"),
	];
}

pages.media = async (main, params) => {
	const room = params.get("room");
	const user = params.get("user") || "";
	const items = [];
	let title = "Media";

	if (room) {
		title = `Media in ${room}`;
		const res = await api("GET", `/_synapse/admin/v1/room/${enc(room)}/media`);
		for (const mxc of [...res.local, ...res.remote]) items.push({ mxc });
	} else if (user) {
		title = `Uploads by ${user}`;
		const res = await api("GET", `/_synapse/admin/v1/users/${enc(user)}/media?limit=200&order_by=created_ts&dir=b`);
		for (const m of res.media) items.push({ mxc: `mxc://${user.split(":").slice(1).join(":")}/${m.media_id}`, type: m.media_type, size: m.media_length, name: m.upload_name, created: m.created_ts });
	}

	const lookup = h("input", { placeholder: "@user:server — show their uploads", value: user, onkeydown: (e) => e.key === "Enter" && navigate("media", { user: e.target.value }) });
	main.replaceChildren(
		h("h1", {}, title),
		h("div", { class: "toolbar" }, lookup, h("span", { class: "muted small" }, "or open a room's media from Rooms. Previews are blurred; click to reveal.")),
		room || user ? (items.length ? h("div", { class: "media-grid" }, items.map((m) => mediaItem(m))) : h("p", { class: "muted" }, "No media.")) : null);
}

function mediaItem(m) {
	const [server, mediaId] = m.mxc.replace("mxc://", "").split("/");
	const thumb = h("div", { class: "media-thumb", title: "Click to reveal" }, h("span", { class: "muted small" }, m.type || "media"));
	thumb.addEventListener("click", () => thumb.classList.toggle("revealed"));
	if (!m.type || RASTER.has(m.type)) loadThumb(thumb, server, mediaId);
	const item = h("div", { class: "media-item" },
		thumb,
		h("div", { class: "mono muted" }, m.name || mediaId),
		h("div", { class: "small muted" }, [m.size ? bytes(m.size) : null, m.created ? date(m.created) : null].filter(Boolean).join(" · ")),
		h("button", { class: "danger small", onclick: async () => {
			if (!(await ask("Delete this media?", { text: m.mxc, confirm: "Delete", danger: true }))) return;
			if (await attempt("Deleted", () => api("DELETE", `/_synapse/admin/v1/media/${enc(server)}/${enc(mediaId)}`))) item.remove();
		} }, "Delete"));
	return item;
}

const RASTER = new Set(["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif", "image/bmp"]);

// Thumbnails need the access token, which <img> can't send, so fetch them
// and show a blob: URL.
async function loadThumb(el, server, mediaId) {
	try {
		const res = await fetch(`/_matrix/client/v1/media/thumbnail/${enc(server)}/${enc(mediaId)}?width=320&height=240&method=scale&allow_remote=false`,
			{ headers: { Authorization: `Bearer ${sessionStorage.getItem(TOKEN_KEY)}` }, credentials: "omit" });
		if (!res.ok) return;
		// Only raster images become blob: URLs; an SVG or HTML one would be a
		// document on the panel's own origin.
		const blob = await res.blob();
		if (!RASTER.has(blob.type)) return;
		const url = URL.createObjectURL(blob);
		el.replaceChildren(h("img", { src: url, alt: "" }));
	} catch { /* leave the placeholder */ }
}

pages.federation = async (main, params) => {
	const from = Number(params.get("from") || 0);
	const res = await api("GET", `/_synapse/admin/v1/federation/destinations?from=${from}&limit=${PAGE_SIZE}`);
	const rows = res.destinations.map((d) => h("tr", {},
		h("td", { class: "mono" }, d.destination),
		h("td", {}, d.failure_ts ? h("span", { class: "tag bad" }, "failing") : h("span", { class: "tag ok" }, "ok")),
		h("td", { class: "small muted" }, d.retry_last_ts ? date(d.retry_last_ts) : "—"),
		h("td", { class: "actions" }, h("button", { class: "ghost small", onclick: () =>
			attempt("Connection reset", () => api("POST", `/_synapse/admin/v1/federation/destinations/${enc(d.destination)}/reset_connection`, {})) }, "Reset"))));
	main.replaceChildren(
		h("h1", {}, "Federation"),
		table(["Server", "State", "Last retry", ""], rows, "No other servers yet."),
		pager(from, res.total, (f) => navigate("federation", { from: f })));
};

pages.console = async (main) => {
	const input = h("textarea", { placeholder: "e.g. server uptime, users list-users, media delete-past-remote-media -b 7d", spellcheck: "false" });
	const output = h("pre", { class: "output", hidden: true });
	const runIt = async () => {
		const cmd = input.value.trim().replace(/^!admin\s+/, "");
		if (!cmd) return;
		output.hidden = false;
		output.textContent = "Running…";
		const r = await attempt(null, () => command(cmd));
		output.textContent = r ? (r.output || (r.ok ? "Done." : "Failed.")) : "Failed.";
	};
	input.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) runIt(); });
	main.replaceChildren(
		h("h1", {}, "Console"),
		h("p", { class: "muted" }, "Runs server admin commands. Try \"help\". Ctrl+Enter runs."),
		input,
		h("div", { class: "toolbar" }, h("button", { onclick: runIt }, "Run")),
		output);
};

pages.notices = async (main) => {
	const res = await api("GET", "/_anoncat/admin/v1/notices");
	const items = (res.notices || []).map((n) =>
		h("div", { class: `card notice${n.loud ? " loud" : ""}` },
			h("div", { class: "small muted" }, date(n.ts)),
			h("pre", { class: "notice-body" }, n.body)));
	main.replaceChildren(
		h("h1", {}, "Notices"),
		h("p", { class: "muted" }, "What the server reports: new users, cleanup runs, plugin failures, reports. Highlighted ones need a look."),
		items.length ? h("div", { class: "notices" }, items) : h("p", { class: "muted" }, "Nothing yet."));
};

// --- AI moderation -------------------------------------------------------------

const MOD = "/_anoncat/admin/v1/moderation";

pages.reports = async (main, params) => {
	const all = params.get("all") === "1";
	const res = await api("GET", `${MOD}/reports${all ? "?all=true" : ""}`);
	const refresh = () => render();
	const sevTag = (s) => h("span", { class: `tag ${s === "high" ? "bad" : s === "medium" ? "warn" : ""}` }, s);
	const items = res.reports.map((r) => {
		const act = (action, done) => attempt(done, () => api("POST", `${MOD}/reports/${r.id}`, { action })).then((x) => x && refresh());
		const buttons = [];
		if (r.status === "open") {
			if (r.mxc) buttons.push(r.hidden
				? h("button", { class: "ghost small", onclick: () => act("unhide", "Shown again") }, "Unhide")
				: h("button", { class: r.suggested === "hide" ? "small" : "ghost small", onclick: () => act("hide", "Hidden") }, "Hide media"));
			if (r.kind !== "large_file") buttons.push(h("button", { class: r.suggested === "redact" ? "danger small" : "ghost small", onclick: async () => {
				if (!(await ask("Remove this message?", { text: "Redacts it as you. Needs you to be in the room with the power to remove messages. Other servers drop their copies too.", confirm: "Remove", danger: true }))) return;
				const ok = await attempt("Removed", () => api("PUT", `/_matrix/client/v3/rooms/${enc(r.room_id)}/redact/${enc(r.event_id)}/${Date.now()}`, { reason: "Moderation" }));
				if (ok) await act("resolve", null);
			} }, "Remove message"));
			buttons.push(h("button", { class: "ghost small", onclick: () => act("dismiss", "Dismissed") }, "Dismiss"));
		} else {
			buttons.push(h("button", { class: "ghost small", onclick: () => act("reopen", "Reopened") }, "Reopen"));
			if (r.hidden) buttons.push(h("button", { class: "ghost small", onclick: () => act("unhide", "Shown again") }, "Unhide"));
		}
		let preview = null;
		if (r.mxc && r.kind === "image") {
			const [server, mediaId] = r.mxc.replace("mxc://", "").split("/");
			preview = h("div", { class: "media-thumb report-thumb", title: "Click to reveal" }, h("span", { class: "muted small" }, "image"));
			preview.addEventListener("click", () => preview.classList.toggle("revealed"));
			loadPreview(preview, server, mediaId);
		}
		return h("div", { class: `card report${r.severity === "high" ? " loud" : ""}` },
			h("div", { class: "report-head" },
				sevTag(r.severity),
				h("span", { class: "tag" }, r.category || r.kind),
				r.hidden ? h("span", { class: "tag warn" }, "hidden") : null,
				r.status !== "open" ? h("span", { class: "tag" }, r.status) : null,
				h("span", { class: "small muted" }, `${date(r.ts)} · ${r.source === "ai" ? "AI" : "size rule"} · suggests ${r.suggested}`)),
			h("div", {}, r.reason),
			preview,
			r.excerpt ? h("pre", { class: "notice-body excerpt" }, r.excerpt) : null,
			h("div", { class: "small muted" }, `${r.sender} in ${r.room_name || r.room_id}`),
			h("div", { class: "toolbar" }, buttons));
	});
	main.replaceChildren(
		h("h1", {}, "Reports"),
		h("p", { class: "muted" }, "What the AI moderation and the size rule flagged. Nothing is removed until you choose to; the AI may only hide images, and only if Settings allow it."),
		h("div", { class: "toolbar" },
			h("button", { class: "ghost small", onclick: () => navigate("reports", all ? {} : { all: "1" }) }, all ? "Show open only" : "Show closed too")),
		items.length ? h("div", { class: "notices" }, items) : h("p", { class: "muted" }, all ? "No reports yet." : "Nothing open."));
};

// Reported images come from the moderation's own preview, which serves them
// even while hidden.
async function loadPreview(el, server, mediaId) {
	try {
		const res = await fetch(`${MOD}/media/${enc(server)}/${enc(mediaId)}`,
			{ headers: { Authorization: `Bearer ${sessionStorage.getItem(TOKEN_KEY)}` }, credentials: "omit", cache: "no-store" });
		if (!res.ok) return;
		const blob = await res.blob();
		if (!RASTER.has(blob.type)) return;
		el.replaceChildren(h("img", { src: URL.createObjectURL(blob), alt: "" }));
	} catch { /* leave the placeholder */ }
}

pages.settings = async (main) => {
	const res = await api("GET", `${MOD}/settings`);
	const s = res.settings;
	const models = await api("GET", `${MOD}/models`).then((r) => r.models).catch(() => []);

	const check = (name, label, help) => {
		const input = h("input", { type: "checkbox", name, checked: s[name] });
		return h("label", { class: "check" }, input, h("span", {}, label, help ? h("span", { class: "muted small block" }, help) : null));
	};
	const modelSelect = (name, vision) => {
		const select = h("select", { name },
			h("option", { value: "auto" }, vision ? "Auto (Venice's default for images)" : "Auto (Venice's default)"),
			models.filter((m) => !vision || m.vision).map((m) => h("option", { value: m.id },
				`${m.id}${m.traits.length ? ` — ${m.traits.join(", ")}` : ""}${m.price_in ? ` — $${m.price_in}/M in` : ""}`)));
		if (s[name] !== "auto" && !models.some((m) => m.id === s[name])) select.append(h("option", { value: s[name] }, s[name]));
		select.value = s[name];
		return select;
	};

	const mode = h("div", { class: "radios" },
		h("label", { class: "check" }, h("input", { type: "radio", name: "media_mode", value: "suggest", checked: s.media_mode === "suggest" }),
			h("span", {}, "Only suggest", h("span", { class: "muted small block" }, "Every flagged image waits in Reports for you."))),
		h("label", { class: "check" }, h("input", { type: "radio", name: "media_mode", value: "hide", checked: s.media_mode === "hide" }),
			h("span", {}, "Hide clearly bad images until I review them", h("span", { class: "muted small block" }, "Hidden images stay stored and come back with one click. Nothing is ever deleted by the AI."))));

	const extra = h("textarea", { name: "extra_rooms", placeholder: "#room:other.server or !roomid:server, one per line", spellcheck: "false" }, s.extra_rooms.join("\n"));
	const rules = h("textarea", { name: "rules", class: "rules" }, s.rules);
	const field = (label, input, help) => h("label", { class: "field" }, h("span", {}, label), input, help ? h("span", { class: "muted small" }, help) : null);
	const batch = h("input", { type: "number", name: "batch_minutes", min: 1, max: 60, value: s.batch_minutes });
	const large = h("input", { type: "number", name: "large_file_mb", min: 0, value: s.large_file_mb });
	const testOut = h("p", { class: "muted small" });

	const form = h("form", { class: "settings" },
		h("section", { class: "card" },
			h("h2", {}, "AI moderation"),
			res.key_saved
				? h("p", { class: "small" }, h("span", { class: "tag ok" }, "key saved"), "Venice API key found.")
				: h("p", { class: "small" }, h("span", { class: "tag warn" }, "no key"), `Save your Venice API key in ${res.key_file || "the file set by ai_key_file"} first; the AI does nothing without it.`),
			check("enabled", "Turn on AI moderation", "Only rooms that aren't encrypted are ever read. Names and IDs are replaced with placeholders (U1, R1) before anything is sent to Venice."),
			h("div", { class: "toolbar" }, h("button", { type: "button", class: "ghost small", onclick: async () => {
				testOut.textContent = "Testing…";
				const r = await attempt(null, () => api("POST", `${MOD}/test`, {}));
				testOut.textContent = r ? r.message : "Test failed.";
			} }, "Test Venice connection")),
			testOut),
		h("section", { class: "card" },
			h("h2", {}, "Which rooms"),
			check("watch_directory", "Rooms in this server's public room list"),
			check("watch_public_joined", "Any room anyone can join that someone here is in", "Includes public rooms on other servers that people here joined."),
			field("Extra rooms", extra, "Rooms from anywhere in the public room space. The server only sees a room once someone here is in it, so join it with your account first.")),
		h("section", { class: "card" },
			h("h2", {}, "What to check"),
			check("check_messages", "Messages"),
			check("check_images", "Images (up to 8 MiB)"),
			field("Report files at least this big (MiB, 0 = off)", large, "Large files get a report for you to look at, without the AI."),
			field("Check new messages every (minutes)", batch, "Messages are checked together, which is cheaper. 40 waiting messages are checked at once.")),
		h("section", { class: "card" },
			h("h2", {}, "Images the AI judges clearly bad"),
			mode),
		h("section", { class: "card" },
			h("h2", {}, "Models and rules"),
			field("Text model", modelSelect("text_model", false)),
			field("Image model", modelSelect("vision_model", true), "Venice runs the open models (Qwen, DeepSeek, GLM, Gemma, Llama, Venice) itself without keeping anything. Gemini, Grok, Claude and GPT get passed on to their own companies; avoid them for privacy."),
			field("Server rules, as the AI is told them", rules)),
		h("div", { class: "toolbar" }, h("button", { type: "submit" }, "Save settings")));

	form.addEventListener("submit", async (e) => {
		e.preventDefault();
		const f = new FormData(form);
		const body = {
			enabled: f.has("enabled"),
			watch_directory: f.has("watch_directory"),
			watch_public_joined: f.has("watch_public_joined"),
			check_messages: f.has("check_messages"),
			check_images: f.has("check_images"),
			media_mode: f.get("media_mode") || "suggest",
			extra_rooms: String(f.get("extra_rooms") || "").split(/\s+/).filter(Boolean),
			large_file_mb: Number(f.get("large_file_mb") || 0),
			batch_minutes: Number(f.get("batch_minutes") || 2),
			text_model: f.get("text_model"),
			vision_model: f.get("vision_model"),
			rules: f.get("rules"),
		};
		if (await attempt("Saved", () => api("PUT", `${MOD}/settings`, body))) render();
	});

	main.replaceChildren(h("h1", {}, "Settings"), form);
};

// The assistant's chat lives only in this tab.
const chat = [];
let redrawChat = () => {};

pages.assistant = async (main) => {
	const log = h("div", { class: "chat" });
	const input = h("textarea", { placeholder: "Ask about the server, e.g. \"how busy is it?\" or \"how do I stop registrations?\"" });
	const draw = () => {
		log.replaceChildren(...chat.map((t) => t.shared
			? h("div", { class: "card bubble shared" },
				h("div", { class: "small muted" }, `Output of ${t.command}, shared with the assistant`),
				h("pre", { class: "notice-body excerpt" }, t.output))
			: h("div", { class: `card bubble ${t.role}` }, renderReply(t.content, t.role === "assistant"))));
		log.scrollTop = log.scrollHeight;
	};
	const send = async () => {
		const text = input.value.trim();
		const last = chat[chat.length - 1];
		// An empty Send right after shared output asks about that output.
		if (!text && !last?.shared) return;
		input.value = "";
		if (text) chat.push({ role: "user", content: text });
		draw();
		log.append(h("p", { class: "muted small" }, "Thinking…"));
		const messages = chat.map(({ role, content }) => ({ role, content }));
		const r = await attempt(null, () => api("POST", `${MOD}/assistant`, { messages }));
		chat.push({ role: "assistant", content: r ? r.reply : "(no answer)" });
		draw();
	};
	redrawChat = draw;
	input.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) send(); });
	main.replaceChildren(
		h("h1", {}, "Assistant"),
		h("p", { class: "muted" }, "Chats with Venice AI. It sees a snapshot of the server's state each time you send, with people and rooms replaced by placeholders. It can propose admin commands; they only run when you click Run and confirm, and their output is shared with it. Ctrl+Enter sends."),
		log, input,
		h("div", { class: "toolbar" },
			h("button", { onclick: send }, "Send"),
			h("button", { class: "ghost", onclick: () => { chat.length = 0; draw(); } }, "Clear")));
	draw();
};

// Characters of a command's output shared with the assistant.
const SHARED_OUTPUT = 6000;

// Plain text, with ```command blocks turned into commands to run on request.
function renderReply(text, commands) {
	const parts = [];
	const re = /```command\s*\n([\s\S]*?)```/g;
	let last = 0, m;
	while (commands && (m = re.exec(text))) {
		if (m.index > last) parts.push(h("pre", { class: "notice-body" }, text.slice(last, m.index).trim()));
		const cmd = m[1].trim().replace(/^!admin\s+/, "");
		parts.push(h("div", { class: "proposed" },
			h("code", { class: "mono" }, cmd),
			h("button", { class: "small", onclick: async () => {
				if (!(await ask("Run this command?", { text: cmd, confirm: "Run" }))) return;
				const r = await attempt(null, () => command(cmd));
				const output = r ? (r.output || (r.ok ? "Done." : "Failed.")) : "Failed.";
				const shared = output.length > SHARED_OUTPUT ? `${output.slice(0, SHARED_OUTPUT)}\n… (cut)` : output;
				chat.push({ role: "user", shared: true, command: cmd, output: shared,
					content: `I ran \`${cmd}\`. Output:\n\`\`\`\n${shared}\n\`\`\`` });
				redrawChat();
				toast("Output shared. Press Send to ask about it.");
			} }, "Run")));
		last = re.lastIndex;
	}
	if (last < text.length) parts.push(h("pre", { class: "notice-body" }, text.slice(last).trim()));
	return parts;
}

// --- routing & session ---------------------------------------------------------

function navigate(page, params = {}) {
	const query = new URLSearchParams(Object.entries(params).filter(([, v]) => v !== "" && v !== undefined && v !== null));
	location.hash = `#/${page}${query.toString() ? `?${query}` : ""}`;
}

async function render() {
	const [path, query] = location.hash.replace(/^#\/?/, "").split("?");
	const page = Object.hasOwn(pages, path) ? path : "dashboard";
	for (const a of document.querySelectorAll(".side a")) a.classList.toggle("active", a.dataset.page === page);
	const main = $("#main");
	main.replaceChildren(h("p", { class: "muted" }, "Loading…"));
	try {
		await pages[page](main, new URLSearchParams(query || ""));
	} catch (e) {
		main.replaceChildren(h("h1", {}, "Couldn't load this page"), h("p", { class: "error" }, e.message));
	}
}

function showApp() {
	$("#login").hidden = true;
	$("#app").hidden = false;
	$("#whoami").textContent = sessionStorage.getItem(USER_KEY) || "";
	render();
}

function showLogin(message = "") {
	$("#app").hidden = true;
	$("#login").hidden = false;
	$("#login-error").textContent = message;
}

// Every sign-in is a new device with its own admin token. A tab closed
// without signing out leaves that token valid, so each sign-in removes the
// panel's earlier devices: at most one panel token is ever live.
async function revokeOldSessions(userId, current) {
	const list = await api("GET", `/_synapse/admin/v2/users/${enc(userId)}/devices`);
	const old = (list.devices || [])
		.filter((d) => d.display_name === PANEL_DEVICE && d.device_id !== current)
		.map((d) => d.device_id);
	if (old.length) await api("POST", `/_synapse/admin/v2/users/${enc(userId)}/delete_devices`, { devices: old });
}

async function signOut(callServer = true) {
	if (callServer) await api("POST", "/_matrix/client/v3/logout", {}).catch(() => {});
	sessionStorage.removeItem(TOKEN_KEY);
	sessionStorage.removeItem(USER_KEY);
	sessionStorage.removeItem(DEVICE_KEY);
	showLogin(callServer ? "" : "Session ended; sign in again.");
}

$("#login-form").addEventListener("submit", async (e) => {
	e.preventDefault();
	const form = new FormData(e.target);
	$("#login-error").textContent = "";
	try {
		const res = await api("POST", "/_matrix/client/v3/login", {
			type: "m.login.password",
			identifier: { type: "m.id.user", user: form.get("user") },
			password: form.get("password"),
			initial_device_display_name: PANEL_DEVICE,
		});
		sessionStorage.setItem(TOKEN_KEY, res.access_token);
		sessionStorage.setItem(USER_KEY, res.user_id);
		sessionStorage.setItem(DEVICE_KEY, res.device_id);
		e.target.reset();
		try {
			await api("GET", "/_anoncat/admin/v1/status");
			await revokeOldSessions(res.user_id, res.device_id);
		} catch {
			await signOut(true);
			return showLogin("That account isn't a server admin.");
		}
		showApp();
	} catch (err) {
		showLogin(err.status === 403 ? "Wrong username or password." : err.message);
	}
});

$("#logout").addEventListener("click", () => signOut(true));
window.addEventListener("hashchange", () => sessionStorage.getItem(TOKEN_KEY) && render());

if (sessionStorage.getItem(TOKEN_KEY)) showApp();
else showLogin();
