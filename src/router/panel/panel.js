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
function ask(title, { text, pre, content, fields = [], confirm = "OK", danger = false, cancel = true } = {}) {
	const dialog = $("#dialog");
	const body = $("#dialog-body");
	const inputs = {};
	dialog.classList.toggle("wide", Boolean(content));
	fill(body,
		h("h2", {}, title),
		text ? h("p", {}, text) : null,
		pre ? h("pre", { class: "notice-body mono" }, pre) : null,
		content || null,
		...fields.map((f) => {
			const input = f.type === "checkbox"
				? h("input", { type: "checkbox", name: f.name, checked: f.value })
				: h("input", { name: f.name, type: f.type || "text", value: f.value ?? "", placeholder: f.placeholder, required: f.required });
			inputs[f.name] = input;
			return h("label", {}, f.label, input);
		}),
		h("div", { class: "buttons" },
			cancel ? h("button", { value: "cancel", class: "ghost", formnovalidate: true }, "Cancel") : null,
			h("button", { value: "ok", class: danger ? "danger" : "" }, confirm)));
	return new Promise((resolve) => {
		dialog.onclose = () => {
			if (dialog.returnValue !== "ok") return resolve(null);
			const values = {};
			for (const [name, input] of Object.entries(inputs)) values[name] = input.type === "checkbox" ? input.checked : input.value;
			resolve(values);
		};
		// Enter in a one-line field confirms, as the button would; without
		// this the browser takes the first button (Cancel) and the note was
		// lost. In a text box, Enter still makes a new line.
		body.onkeydown = (e) => {
			if (e.key !== "Enter" || e.shiftKey || e.isComposing) return;
			const t = e.target;
			if (t.tagName !== "INPUT" || ["checkbox", "radio", "file", "button", "submit"].includes(t.type)) return;
			e.preventDefault();
			if (!body.reportValidity()) return;
			dialog.close("ok");
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

// Like replaceChildren, which would write a left-out (null) part as the
// text "null".
function fill(el, ...children) {
	el.replaceChildren(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
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

	fill(main, 
		h("h1", {}, "Dashboard"),
		h("div", { class: "grid" },
			stat("Disk (vault)", diskUsed !== null ? `${((diskUsed / s.disk_total) * 100).toFixed(0)}%` : "—", `${bytes(diskUsed)} of ${bytes(s.disk_total)}`, s.disk_total ? meter(diskUsed / s.disk_total) : null),
			stat("System memory", ramUsed !== null ? `${((ramUsed / s.ram_total) * 100).toFixed(0)}%` : "—", `${bytes(s.ram_available)} available`, s.ram_total ? meter(ramUsed / s.ram_total) : null),
			serverMemory(s),
			reachableStat(s.reachable),
			stat("Uptime", duration(s.uptime_secs), s.version)),
		h("h2", {}, "Cleanup"),
		table(["Rule", "Setting"], [
			["Runs every", duration(c.interval_secs)],
			["Remote media kept for", duration(c.remote_media_max_age_secs)],
			["Local uploads kept for", duration(c.local_media_max_age_secs)],
			["Local uploads size cap", c.local_media_max_size ? bytes(c.local_media_max_size) : "off"],
			["Message history kept for", duration(c.message_max_age_secs)],
			["Messages with media kept for", duration(Math.max(c.media_message_max_age_secs, c.message_max_age_secs))],
			["Disk guard at", c.disk_percent ? `${c.disk_percent}%` : "off"],
			["Low-memory guard below", c.min_free_ram ? bytes(c.min_free_ram) : "off"],
		].map(([k, v]) => h("tr", {}, h("td", {}, k), h("td", {}, v)))),
		h("p", { class: "muted small" }, "Change these, run a cleanup and see past runs on the ", h("a", { href: "#/cleanup" }, "Cleanup"), " page."),
		h("div", { class: "toolbar" },
			run("Clear caches", "server clear-caches"),
			run("Memory details", "server memory-usage")),
		output);
};

// Whether the public address answered, checked every 5 minutes through the
// VPN and the relay, as anyone outside would.
function reachableStat(r) {
	if (!r || r.ok === null || r.ok === undefined) return stat("Reachable from outside", "\u2014", "first check runs soon after start");
	const card = stat("Reachable from outside", r.ok ? "Yes" : "No",
		r.ok ? `since ${date(r.since_ts)}` : `${r.error || "failed"} \u00B7 since ${date(r.since_ts)}`);
	if (!r.ok) card.classList.add("warn");
	card.title = `Last checked ${date(r.checked_ts)}`;
	return card;
}

// The server's memory against its limits: past the soft cap it's slowed
// while the kernel takes memory back, so sitting near it costs speed.
function serverMemory(s) {
	if (!s.memory_current || !s.memory_high) return stat("Server memory", bytes(s.rss), "resident");
	const share = s.memory_current / s.memory_high;
	const held = s.memory_high_events ? `, held back ${s.memory_high_events}\u00D7` : "";
	const note = `${bytes(s.memory_current)} of ${bytes(s.memory_high)} soft cap${s.memory_max ? `, ${bytes(s.memory_max)} hard` : ""}${held}`;
	const card = stat("Server memory", `${(share * 100).toFixed(0)}%`, note, meter(Math.min(share, 1)));
	if (share >= 0.9 || s.memory_high_events) card.classList.add("warn");
	card.title = "Includes the server's caches. Near the soft cap the kernel squeezes it and it gets slower; lower the cache sizes in the config if it lives there.";
	return card;
}

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

	fill(main, 
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
		h("button", { class: "danger small", onclick: async () => { if (await blockTarget(u.name)) refresh(); } }, "Block"),
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
		h("button", { class: "danger small", title: "Deletes every file they uploaded; their account stays", onclick: async () => {
			const media = await attempt(null, () => api("GET", `/_synapse/admin/v1/users/${id}/media?limit=1`));
			if (!media) return;
			if (!media.total) return toast("They haven't uploaded anything");
			if (!(await ask(`Delete ${u.name}'s uploads?`, { text: `All ${media.total} files they uploaded are deleted for good, for everyone. Their account stays.`,
				confirm: "Delete uploads", danger: true }))) return;
			const r = await attempt(null, () => api("DELETE", `/_synapse/admin/v1/users/${id}/media`));
			if (r) toast(`Deleted ${r.total} file${r.total === 1 ? "" : "s"}`);
		} }, "Delete uploads"),
		h("button", { class: "danger small", title: "Removes everything they said, in every room they were ever in, and the files they posted", onclick: async () => {
			if (!(await ask(`Remove ${u.name}'s messages?`, { text: "Everything they said and posted, in every room they were ever in, is removed for everyone, including people on other servers. Files they posted are deleted and show as the stand-in. Their account stays.",
				confirm: "Remove messages", danger: true }))) return;
			removeMessages(u.name);
		} }, "Remove messages"),
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

	fill(main, 
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

	fill(main, 
		h("h1", {}, "Rooms"),
		h("div", { class: "toolbar" }, h("input", { placeholder: "Search rooms", value: q, onkeydown: (e) => e.key === "Enter" && go({ q: e.target.value, from: 0 }) }),
			h("span", { class: "muted small" }, "\u201cList publicly\u201d puts a room on this server's public list. For a room not shown here, paste its #address or !ID into Look up.")),
		table(["Room", "Members", "", ""], rows, "No rooms match."),
		pager(from, res.total_rooms, (f) => go({ from: f })));
};

// Puts a room on this server's public room list, or takes it off, whoever
// made it. Only this server's list changes; the room's own settings and
// other servers' lists stay as they are.
function listButton(roomId, listed, name, refresh) {
	return h("button", { class: "ghost small", title: listed ? "Take it off this server's public room list" : "Show it in this server's public room list, without its creator doing anything", onclick: async () => {
		if (!listed) {
			const ok = await ask(`List ${name || roomId} publicly?`, {
				text: "Anyone browsing this server's public rooms will see it and can ask to join. Only this server's list changes; whether people can actually join still depends on the room's own settings.",
				confirm: "List it" });
			if (!ok) return;
		}
		const r = await attempt(null, () => command(`rooms directory ${listed ? "unpublish" : "publish"} ${roomId}`));
		if (!r) return;
		if (!r.ok) return toast(`Couldn't change the listing: ${r.output.replace(/<[^>]*>/g, "").trim()}`);
		toast(listed ? "Taken off the public list" : "Listed publicly");
		refresh?.();
	} }, listed ? "Unlist" : "List publicly");
}

function roomActions(r, refresh) {
	const id = enc(r.room_id);
	return [
		h("button", { class: "small", onclick: () => lookAt({ kind: "room", room_id: r.room_id }, r.name || r.canonical_alias || "Room") }, "Look"),
		listButton(r.room_id, r.public, r.name || r.canonical_alias, refresh),
		h("button", { class: "ghost small", onclick: () => navigate("media", { room: r.room_id }) }, "Media"),
		h("button", { class: "ghost small", onclick: async () => {
			const v = await ask(`Delete history in ${r.name || r.room_id}`, {
				text: "Deletes this server's copy of messages before the date. Room state is kept; other servers keep their copies.",
				fields: [{ name: "days", label: "Older than days", type: "number", value: "30", required: true }], confirm: "Delete history", danger: true });
			if (!v) return;
			const ts = Date.now() - Number(v.days) * 86400000;
			await attempt("History purge started", () => api("POST", `/_synapse/admin/v1/purge_history/${id}`, { purge_up_to_ts: ts, delete_local_events: true }));
		} }, "Purge history"),
		h("button", { class: "ghost small", onclick: async () => { if (await blockRoom(r.room_id, r.name)) setTimeout(refresh, 1000); } }, "Block…"),
		h("button", { class: "danger small", onclick: async () => {
			const v = await ask(`Delete ${r.name || r.room_id}?`, {
				text: "Removes every local user from the room and deletes it from this server.",
				fields: [{ name: "block", label: "Block it so nobody here can rejoin", type: "checkbox", value: true }], confirm: "Delete room", danger: true });
			if (!v) return;
			const started = await attempt(null, () => api("DELETE", `/_synapse/admin/v2/rooms/${id}`, { block: v.block, purge: true }));
			if (!started) return;
			toast("Deleting the room…");
			await roomDeleted(started.delete_id);
			refresh();
		} }, "Delete"),
	];
}

// Waits up to two minutes for a room delete to finish, and says how it went.
async function roomDeleted(deleteId) {
	for (let i = 0; i < 120; i++) {
		await new Promise((done) => setTimeout(done, 1000));
		const s = await api("GET", `/_synapse/admin/v2/rooms/delete_status/${enc(deleteId)}`).catch(() => null);
		if (s?.status === "complete") return toast("Room deleted");
		if (s?.status === "failed") return toast(`Room delete failed${s.error ? `: ${s.error}` : ""}`);
	}
	toast("Still deleting; it'll leave the list when it's done");
}

pages.media = async (main, params) => {
	if (params.get("view") === "remote") return remoteMedia(main, params);
	if (params.get("view") === "storage") return storageCheck(main, params);
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
	let list;
	if (room || user) {
		const grid = h("div", { class: "media-grid" }, items.map((m) => mediaItem(m)));
		list = items.length
			? h("div", {}, bulkBar(grid, { remote: false, onDone: () => render() }), grid)
			: h("p", { class: "muted" }, room
				? "No media found. Encrypted rooms don't count: the server can't see which files their messages point to."
				: "No uploads.");
	} else {
		list = await uploaders();
	}
	fill(main, 
		h("h1", {}, title),
		mediaTabs("here"),
		h("div", { class: "toolbar" }, lookup, h("span", { class: "muted small" }, "or open a room's media from Rooms. Previews are blurred; click to reveal.")),
		list);
}

// Takes a file off the screen everywhere it's shown, the moment it's
// swapped, hidden or deleted: its previews (and anything playing) become a
// plain label, and a full-size view of it closes. Nothing bad stays in front
// of you until the next reload.
function blankMedia(mxc, label) {
	for (const el of document.querySelectorAll(".media-thumb[data-mxc]")) {
		if (el.dataset.mxc !== mxc) continue;
		for (const m of el.querySelectorAll("video, audio")) { m.pause(); m.removeAttribute("src"); m.load(); }
		el.classList.remove("revealed", "playing");
		el.classList.add("gone");
		el.replaceChildren(h("span", { class: "thumb-label" }, label));
		el._load = null;
	}
	const dialog = $("#dialog");
	if (dialog.open && dialog.dataset.mxc === mxc) dialog.close();
}

// Ticking several files and acting on them at once, and filters for the
// grid. Swapping deletes each file for good and serves the stand-in; deleting
// a copy from another server only frees the space here.
function bulkBar(grid, { remote, onDone }) {
	const items = () => [...grid.querySelectorAll(".media-item[data-mxc]")];
	const picked = () => items().filter((el) => !el.hidden && el.querySelector(".pick")?.checked);
	const count = h("span", { class: "muted small" });
	const update = () => {
		const n = picked().length;
		count.textContent = n ? `${n} selected` : "None selected";
		for (const b of buttons) b.disabled = !n;
	};
	for (const el of items()) {
		const box = h("input", { type: "checkbox", class: "pick", "aria-label": "Select", onchange: () => { el.classList.toggle("picked", box.checked); update(); } });
		box.addEventListener("click", (e) => e.stopPropagation());
		el.prepend(h("label", { class: "pick-wrap", title: "Select" }, box));
	}
	const run = async (label, verb, fn) => {
		const chosen = picked();
		if (!chosen.length) return;
		if (!(await ask(`${label} ${chosen.length} files?`, { text: verb, confirm: label, danger: true }))) return;
		let done = 0, failed = 0;
		const queue = [...chosen];
		const worker = async () => {
			for (let el = queue.shift(); el; el = queue.shift()) {
				const [server, mediaId] = el.dataset.mxc.replace("mxc://", "").split("/");
				const path = `/_anoncat/admin/v1/media/item/${enc(server)}/${enc(mediaId)}`;
				try { await fn(path); done++; blankMedia(el.dataset.mxc, label === "Swap" ? "Swapped for the stand-in" : "Deleted"); } catch { failed++; }
				count.textContent = `${done + failed} of ${chosen.length}\u2026`;
			}
		};
		await Promise.all([worker(), worker(), worker()]);
		toast(failed ? `${done} done, ${failed} failed` : `${done} done`);
		onDone?.();
	};
	const buttons = [
		h("button", { type: "button", class: "danger small", onclick: thenAsk(() => run("Swap", "Each file is deleted for good; everyone, here and on other servers, gets the stand-in picture instead.", (p) => api("POST", `${p}/hide`))) }, "Swap for stand-in"),
		h("button", { type: "button", class: "ghost small", onclick: thenAsk(() => run("Delete", remote
			? "Only this server's copies go. Anything someone here opens again is fetched again, unless it's swapped or its server is blocked."
			: "These uploads are deleted for good, for everyone.", (p) => api("DELETE", p))) }, remote ? "Delete copies" : "Delete"),
	];
	const FILTERS = [["", "All"], ["image", "Pictures"], ["video", "Videos"], ["notinroom", "Not seen in a room"], ["hidden", "Hidden"], ["swapped", "Swapped"]];
	const filter = h("select", { "aria-label": "Show", onchange: () => {
		const want = filter.value;
		for (const el of items()) el.hidden = Boolean(want) && !(el.dataset.flags || "").split(" ").includes(want);
		update();
	} }, FILTERS.filter(([key]) => remote || !["notinroom", "hidden", "swapped"].includes(key)).map(([key, label]) => h("option", { value: key }, label)));
	const all = h("button", { type: "button", class: "ghost small", onclick: () => {
		const shown = items().filter((el) => !el.hidden);
		const on = !shown.every((el) => el.querySelector(".pick").checked);
		for (const el of shown) { el.querySelector(".pick").checked = on; el.classList.toggle("picked", on); }
		update();
	} }, "Select all shown");
	// Bigger previews, remembered on this browser.
	const BIG = "anoncat-big-previews";
	const big = (() => { try { return localStorage.getItem(BIG) === "1"; } catch { return false; } })();
	grid.classList.toggle("large", big);
	const size = h("button", { type: "button", class: "ghost small", onclick: () => {
		const on = !grid.classList.contains("large");
		grid.classList.toggle("large", on);
		size.textContent = on ? "Smaller previews" : "Bigger previews";
		try { localStorage.setItem(BIG, on ? "1" : "0"); } catch {}
	} }, big ? "Smaller previews" : "Bigger previews");
	update();
	return h("div", { class: "toolbar bulk" }, filter, all, count, ...buttons, size);
}

function mediaTabs(current) {
	const tab = (key, label, params) => h("button", { type: "button", class: key === current ? "small" : "ghost small",
		onclick: () => navigate("media", params) }, label);
	return h("div", { class: "toolbar" },
		tab("here", "Uploaded here", {}),
		tab("remote", "From other servers", { view: "remote" }),
		tab("storage", "Storage check", { view: "storage" }));
}

// Accounts here using the server as file storage: uploading far more than
// they post, or uploading pieces of split-up files.
async function storageCheck(main, params) {
	const res = await api("GET", "/_anoncat/admin/v1/media/storage");
	const reload = () => storageCheck(main, params);
	const all = params.get("all") === "1";
	const flagged = res.uploaders.filter((u) => u.flagged);
	const shown = all ? res.uploaders : flagged;
	const idx = res.index;

	const remove = async (u, onlyPieces) => {
		const what = onlyPieces ? `${u.pieces} split-file pieces (${bytes(u.piece_bytes)})` : `all ${u.uploads} uploads (${bytes(u.bytes)})`;
		if (!(await ask(`Delete ${what} from ${u.user_id}?`, {
			text: onlyPieces
				? "Only files named like pieces (chunk_0.enc and so on) go. They're gone for good, for everyone."
				: "Every file they uploaded goes, for good, including pictures they posted in rooms. Files in encrypted rooms can't be told apart from stored ones, so this is the sure way.",
			confirm: "Delete", danger: true }))) return;
		const r = await attempt(null, () => api("POST", "/_anoncat/admin/v1/media/storage/delete", { user_id: u.user_id, only_pieces: onlyPieces }));
		if (r) { toast(`Deleted ${r.items} files, ${bytes(r.bytes)}`); reload(); }
	};

	const rows = shown.map((u) => h("tr", {},
		h("td", {}, h("a", { href: "#", onclick: (e) => { e.preventDefault(); navigate("media", { user: u.user_id }); } }, u.user_id),
			u.flagged ? h("span", { class: "tag bad" }, "suspicious") : null,
			u.reasons.length ? h("div", { class: "small muted" }, u.reasons.join("; ")) : null),
		h("td", {}, String(u.uploads)),
		h("td", {}, bytes(u.bytes)),
		h("td", {}, String(u.in_rooms)),
		h("td", {}, String(u.posts)),
		h("td", {}, u.pieces ? `${u.pieces} (${bytes(u.piece_bytes)})` : "0"),
		h("td", {}, date(u.newest_ts)),
		h("td", { class: "actions" }, h("span", {},
			u.pieces ? h("button", { class: "danger small", onclick: () => remove(u, true) }, "Delete pieces") : null,
			h("button", { class: "danger small", onclick: () => remove(u, false) }, "Delete all uploads"),
			u.user_id !== sessionStorage.getItem(USER_KEY) ? h("button", { class: "ghost small", onclick: thenAsk(async () => { if (await blockTarget(u.user_id)) reload(); }) }, "Block") : null))));

	fill(main,
		h("h1", {}, "Storage check"),
		mediaTabs("storage"),
		h("p", { class: "muted small" },
			"Some tools cut a file into encrypted pieces and upload them to any homeserver without ever posting them, so the server becomes free, invisible storage. ",
			"Real attachments get posted: one message per file, even in encrypted rooms. Accounts that upload far more than they post are listed here."),
		h("section", { class: "card" },
			h("p", {},
				res.upload_guard
					? [h("span", { class: "tag ok" }, "upload limit on"), ` An account may upload ${res.upload_allowance} files a day more than it posts, and pieces of split-up files are refused. Admins aren't limited.`]
					: [h("span", { class: "tag warn" }, "upload limit off"), " Anyone here can upload as much as they like. Turn the limit on in Settings."]),
			h("p", { class: "small muted" },
				idx.scanning ? `Reading past messages for where files were posted… ${idx.events_read.toLocaleString()} so far. The "In rooms" and "Posts" counts fill in as it goes.`
					: idx.indexed ? "Where files were posted is known for every message this server holds. Files posted in encrypted rooms never count as in a room: the server can't read those messages."
					: "Past messages haven't been read yet, so \"In rooms\" and \"Posts\" may be low."),
			h("div", { class: "toolbar" },
				idx.scanning ? h("button", { class: "ghost small", onclick: reload }, "Refresh")
					: h("button", { class: "ghost small", onclick: async () => {
						if (await attempt("Reading past messages", () => api("POST", "/_anoncat/admin/v1/media/storage/reindex"))) setTimeout(reload, 1500);
					} }, "Read past messages again"))),
		h("div", { class: "toolbar" },
			h("button", { class: all ? "ghost small" : "small", onclick: () => navigate("media", { view: "storage" }) }, `Suspicious (${flagged.length})`),
			h("button", { class: all ? "small" : "ghost small", onclick: () => navigate("media", { view: "storage", all: "1" }) }, `Everyone who uploaded (${res.uploaders.length})`)),
		table(["Account", "Files", "Size", "In rooms", "Posts", "Pieces", "Newest", ""], rows,
			all ? "Nobody here has uploaded anything." : "Nothing suspicious: everyone's uploads match what they post."));
}

// Copies of other servers' media, kept here because someone here viewed it.
async function remoteMedia(main, params) {
	const only = params.get("server") || "";
	const res = await api("GET", `/_anoncat/admin/v1/media/cached${only ? `?server=${enc(only)}` : ""}`);
	const reload = () => remoteMedia(main, params);

	const serverRows = res.servers.map((s) => h("tr", { "data-server": s.server },
		h("td", {}, h("a", { href: "#", onclick: (e) => { e.preventDefault(); navigate("media", { view: "remote", server: s.server }); } }, s.server),
			s.blocked ? h("span", { class: "tag warn" }, "blocked") : null),
		h("td", {}, String(s.items)),
		h("td", {}, bytes(s.bytes)),
		h("td", { class: "actions" }, h("span", {},
			h("button", { class: "danger small", onclick: async () => {
				if (!(await ask(`Delete everything cached from ${s.server}?`, {
					text: `${s.items} files, ${bytes(s.bytes)}. Only this server's copies go; anything someone here opens again is fetched again${s.blocked ? "" : ", unless you block the server"}.`,
					confirm: "Delete", danger: true }))) return;
				const r = await attempt(null, () => api("POST", "/_anoncat/admin/v1/media/cached/purge", { server: s.server }));
				if (r) { toast(`Deleted ${r.items} files, ${bytes(r.bytes)}`); reload(); }
			} }, "Delete all"),
			s.blocked ? null : h("button", { class: "ghost small", onclick: thenAsk(async () => { if (await blockTarget(s.server)) reload(); }) }, "Block server")))));

	const grid = res.media.length
		? h("div", { class: "media-grid" }, res.media.map((m) => cachedItem(m, res.servers.some((s) => s.server === m.server && s.blocked))))
		: h("p", { class: "muted" }, only ? "Nothing cached from this server." : "Nothing cached from other servers.");
	const bulk = res.media.length ? bulkBar(grid, { remote: true, onDone: reload }) : null;

	// The AI's pass over what's already stored: this server's, or everyone's.
	const aiLine = h("span", { class: "muted small" });
	const aiButton = h("button", { class: "small", type: "button", onclick: thenAsk(async () => {
		const what = only ? `everything stored from ${only}` : "everything stored from other servers";
		if (!(await ask("Have the AI check these?", {
			text: `Each picture and video in ${what} is checked once, one at a time, in the background: pictures as a whole and in parts, videos by six frames. What it flags goes to Reports. Swapped, hidden and blocked servers' media is skipped. Each one is a call to the AI service.`,
			confirm: "Start" }))) return;
		const r = await attempt("Started; flagged ones go to Reports", () => api("POST", `${MOD}/backlog`, { server: only }));
		if (r) pollAi();
	}) }, only ? `AI check everything from ${only}` : "AI check everything stored");
	const pollAi = async () => {
		const st = await api("GET", `${MOD}/backlog`).catch(() => null);
		if (!st || !aiLine.isConnected) return;
		aiButton.disabled = st.running;
		aiLine.textContent = st.running ? `Checking: ${st.checked} of ${st.total}, ${st.found} flagged so far.`
			: st.finished_ts ? `Last check ${date(st.finished_ts)}: ${st.checked} checked, ${st.found} flagged.` : "";
		if (st.running) setTimeout(pollAi, 4000);
	};
	pollAi();

	fill(main,
		h("h1", {}, only ? `Media from ${only}` : "Media from other servers"),
		mediaTabs("remote"),
		h("p", { class: "muted small" },
			`${res.items} files, ${bytes(res.bytes)} in all. These are copies kept here after someone on this server viewed them. `,
			"Deleting a copy frees the space, but it's fetched again if someone opens it, unless its server is blocked. ",
			res.cat_stand_in
				? "Hidden media and media from blocked servers show up for people here as the stand-in picture chosen in Settings."
				: "Stand-in pictures are off in Settings, so hidden and blocked media shows as a broken image."),
		h("div", { class: "toolbar" },
			only ? h("button", { class: "ghost small", onclick: () => navigate("media", { view: "remote" }) }, "All servers") : null,
			aiButton, aiLine),
		only ? null : h("h2", {}, "By server"),
		only || res.servers.length < 2 ? null : h("div", { class: "toolbar" }, h("input", { type: "search", placeholder: "Search servers, e.g. cloud", "aria-label": "Search servers",
			oninput: (e) => {
				const words = e.target.value.toLowerCase().split(/\s+/).filter(Boolean);
				for (const row of serverRows) row.hidden = !words.every((w) => row.dataset.server.toLowerCase().includes(w));
			} })),
		only ? null : table(["Server", "Files", "Size", ""], serverRows, "Nothing cached."),
		h("h2", {}, only ? "Files" : res.media.length < res.items ? `Biggest ${res.media.length} files` : "Files"),
		bulk,
		grid);
}

function cachedItem(m, blocked) {
	const [server, mediaId] = m.mxc.replace("mxc://", "").split("/");
	const thumb = mediaThumb(m.mxc, m.media_type);
	const kindOf = (t) => !t ? "" : t.startsWith("image/") ? "image" : t.startsWith("video/") ? "video" : "other";
	const flags = [m.swapped ? "swapped" : "", m.hidden ? "hidden" : "", m.in_room === false ? "notinroom" : "", kindOf(m.media_type)].filter(Boolean).join(" ");
	const item = h("div", { class: "media-item", "data-mxc": m.mxc, "data-flags": flags, "data-remote": "1" },
		thumb,
		h("div", { class: "mono muted" }, mediaId),
		h("div", { class: "small muted" }, [server, bytes(m.bytes), m.fetched_ts ? date(m.fetched_ts) : null].filter(Boolean).join(" · ")),
		m.in_room === false ? h("div", {}, h("span", { class: "tag warn", title: "Nobody posted it in a room here that isn't encrypted. Someone here asked for it directly, or it was posted in an encrypted room." }, "not seen in a room")) : null,
		mediaActions(m.mxc, { info: { local: false, stored: true, hidden: m.hidden, server_blocked: blocked }, onDeleted: () => item.remove() }));
	return item;
}

// Everyone here who uploaded something, biggest first.
async function uploaders() {
	const res = await api("GET", "/_synapse/admin/v1/statistics/users/media?order_by=media_length&dir=b&limit=500");
	const rows = (res.users || []).map((u) => h("tr", {},
		h("td", {}, h("a", { href: "#", onclick: (e) => { e.preventDefault(); navigate("media", { user: u.user_id }); } }, u.user_id)),
		h("td", {}, String(u.media_count)),
		h("td", {}, bytes(u.media_length))));
	return h("div", {},
		h("h2", {}, "Uploaders"),
		table(["User", "Files", "Size"], rows, "Nobody here has uploaded anything."));
}

function mediaItem(m) {
	const [server, mediaId] = m.mxc.replace("mxc://", "").split("/");
	const thumb = mediaThumb(m.mxc, m.type);
	const kind = !m.type ? "" : m.type.startsWith("image/") ? "image" : m.type.startsWith("video/") ? "video" : "other";
	const item = h("div", { class: "media-item", "data-mxc": m.mxc, "data-flags": kind },
		thumb,
		h("div", { class: "mono muted" }, m.name || mediaId),
		h("div", { class: "small muted" }, [m.size ? bytes(m.size) : null, m.created ? date(m.created) : null].filter(Boolean).join(" · ")),
		mediaActions(m.mxc, { onDeleted: () => item.remove() }));
	return item;
}

const RASTER = new Set(["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif", "image/bmp"]);

// A blurred preview of any media: a small still (a frame, for a video),
// fetched once it scrolls into view and only a few at a time, so long lists
// stay quick. Videos and audio play in place.
function mediaThumb(mxc, type, { still = mxc, cls = "", held = false } = {}) {
	const kind = !type || RASTER.has(type) ? "image"
		: type.startsWith("video/") ? "video"
		: type.startsWith("audio/") ? "audio" : "file";
	const label = h("span", { class: "thumb-label" }, { image: type || "media", video: "video", audio: "audio" }[kind] || type);
	const el = h("div", { class: `media-thumb ${kind}${cls ? ` ${cls}` : ""}`, title: "Click to reveal", "data-mxc": mxc }, label);
	el.addEventListener("click", (e) => {
		if (!e.target.closest("video, audio, .play, .vbox")) el.classList.toggle("revealed");
	});
	// Hidden media isn't loaded until asked for: it was hidden for a reason.
	if (held) {
		label.textContent = "Hidden \u2014 click to load the preview";
		el.addEventListener("click", () => { if (!el.querySelector("img")) loadPreview(el, still); }, { once: true });
	} else if (kind === "image" || kind === "video" || still !== mxc) whenSeen(el, () => loadPreview(el, still));
	if (kind === "video" || kind === "audio") {
		el.append(h("button", { class: "play", type: "button", title: `Play the ${kind} here`, "aria-label": `Play the ${kind}`,
			onclick: () => playInline(el, mxc, kind) }, "\u25B6"));
	}
	if (kind === "video") {
		el.append(h("button", { class: "full", type: "button", title: "Play it full size", "aria-label": "Play full size",
			onclick: (e) => { e.stopPropagation(); openFullVideo(mxc); } }, "\u2922"));
	}
	return el;
}

// Runs `load` once the element is close to being on screen.
const seen = "IntersectionObserver" in window ? new IntersectionObserver((entries) => {
	for (const entry of entries) {
		if (!entry.isIntersecting) continue;
		seen.unobserve(entry.target);
		entry.target._load?.();
	}
}, { rootMargin: "300px" }) : null;
function whenSeen(el, load) {
	if (!seen) return void load();
	el._load = load;
	seen.observe(el);
}

// At most this many previews download at once; the rest wait their turn.
const PREVIEWS_AT_ONCE = 3;
const previewQueue = { running: 0, waiting: [] };
function queued(task) {
	return new Promise((resolve) => {
		previewQueue.waiting.push(async () => {
			try { resolve(await task()); } catch { resolve(null); } finally {
				previewQueue.running--;
				nextPreview();
			}
		});
		nextPreview();
	});
}
function nextPreview() {
	while (previewQueue.running < PREVIEWS_AT_ONCE && previewQueue.waiting.length) {
		previewQueue.running++;
		previewQueue.waiting.shift()();
	}
}

const authed = () => ({ headers: { Authorization: `Bearer ${sessionStorage.getItem(TOKEN_KEY)}` }, credentials: "omit", cache: "no-store" });

// The moderation's own preview serves stored media even while hidden, and
// fetches what this server has no copy of from its server. For a picture it
// is made from the file itself, never a thumbnail the sender or another
// server chose (those can be an innocent crop of something that isn't), and
// it says how big the picture is and how many frames it has.
async function loadPreview(el, mxc) {
	const [server, mediaId] = mxc.replace("mxc://", "").split("/");
	const got = await queued(async () => {
		if (!el.isConnected) return null;
		const res = await fetch(`${MOD}/media/${enc(server)}/${enc(mediaId)}?thumb=1`, authed());
		if (!res.ok) return null;
		const num = (name) => Number(res.headers.get(name)) || 0;
		return { blob: await res.blob(), width: num("x-anoncat-width"), height: num("x-anoncat-height"), frames: num("x-anoncat-frames"), sheet: res.headers.get("x-anoncat-sheet") === "1" };
	});
	if (!got) {
		const label = el.querySelector(".thumb-label");
		if (label) label.textContent = "No preview";
		return;
	}
	// Only raster images become blob: URLs; an SVG or HTML one would be a
	// document on the panel's own origin.
	if (!RASTER.has(got.blob.type) || !el.isConnected) return;
	const url = URL.createObjectURL(got.blob);
	el.querySelector(".thumb-label")?.replaceWith(h("img", { src: url, alt: "", onload: () => URL.revokeObjectURL(url) }));
	if (got.sheet) {
		el.append(h("span", { class: "thumb-badge", title: "Six frames from across the whole video, taken from the file itself. Play it to see all of it." }, "frames from across it"));
		// The server found a video in it even where its stated type is
		// missing or wrong (a report on a remote file, an upload sent as
		// application/octet-stream): it can be played all the same.
		if (!el.querySelector(".play")) {
			el.classList.add("video");
			el.append(h("button", { class: "play", type: "button", title: "Play the video here", "aria-label": "Play the video",
				onclick: () => playInline(el, mxc, "video") }, "▶"));
			el.append(h("button", { class: "full", type: "button", title: "Play it full size", "aria-label": "Play full size",
				onclick: (e) => { e.stopPropagation(); openFullVideo(mxc); } }, "⤢"));
		}
	}
	if (got.width && got.height) {
		const long = Math.max(got.width, got.height) >= 2 * Math.min(got.width, got.height);
		const badges = [`${got.width}\u00D7${got.height}`];
		if (got.frames > 1) badges.push(`animated, ${got.frames} frames`);
		if (long) badges.push(got.height > got.width ? "very tall" : "very wide");
		el.append(h("span", { class: `thumb-badge${got.frames > 1 || long ? " warn" : ""}`,
			title: got.frames > 1 || long ? "The small preview can't show all of this. Open it full size before deciding." : "" }, badges.join(" \u00B7 ")));
		if (!got.sheet) el.append(h("button", { class: "full", type: "button", title: "Open the whole file, full size", "aria-label": "Open full size",
			onclick: (e) => { e.stopPropagation(); openFull(mxc); } }, "\u2922"));
	}
}

// The file itself, at full size and moving if it moves, in a scrollable
// window: what someone sees when they open it in an app.
async function openFull(mxc) {
	const [server, mediaId] = mxc.replace("mxc://", "").split("/");
	const res = await fetch(`${MOD}/media/${enc(server)}/${enc(mediaId)}`, authed()).catch(() => null);
	if (!res?.ok) return toast("Couldn't load the file.");
	const blob = await res.blob();
	if (!RASTER.has(blob.type)) return toast("Not a picture the panel can show.");
	const url = URL.createObjectURL(blob);
	const shown = ask("Full size", { content: h("div", { class: "full-view" }, h("img", { src: url, alt: "" })), confirm: "Close", cancel: false });
	$("#dialog").dataset.mxc = mxc;
	await shown;
	delete $("#dialog").dataset.mxc;
	URL.revokeObjectURL(url);
}

// A video or sound file for the panel to play: a short-lived link to it on
// the server, not a blob: URL, which some browsers' players (WebKitGTK's,
// through GStreamer) won't play. The file itself by default, which starts at
// once; `converted` asks the server's video worker for a WebM copy (VP8 and
// Opus) every browser decodes, for a video in a codec this browser can't
// play (HEVC from phones, often), at the cost of a wait while it's made.
async function playableVideo(server, mediaId, converted = false) {
	const r = await api("POST", `${MOD}/media/${enc(server)}/${enc(mediaId)}/play`, { convert: converted });
	if (!r.bytes) throw new Error("the server sent nothing");
	return { url: r.url, converted: r.converted, type: r.media_type || "no type", size: r.bytes };
}

// A player that starts silent: what a video is shows before anything plays
// out loud. The controls turn the sound on.
function videoPlayer(url) {
	const player = h("video", { src: url, autoplay: true, playsinline: true, preload: "auto" });
	player.muted = player.defaultMuted = true;
	return player;
}

// The panel's own controls round a video, not the browser's: WebKitGTK's
// full screen button puts the <video> itself full screen, where its picture
// stops at that frame for good. Full screen here is the box holding the video
// and these controls, which keeps playing.
function videoBox(player) {
	const time = (t) => Number.isFinite(t) ? `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, "0")}` : "0:00";
	const play = h("button", { class: "vbtn", type: "button", title: "Play or pause (space)" }, "▶");
	const seek = h("input", { class: "vseek", type: "range", min: "0", max: "1000", value: "0", step: "1", "aria-label": "Position" });
	const clock = h("span", { class: "vtime mono" }, "0:00 / 0:00");
	const sound = h("button", { class: "vbtn", type: "button", title: "Sound on or off" }, "\u{1F507}");
	const volume = h("input", { class: "vvol", type: "range", min: "0", max: "1", step: "0.05", value: "1", "aria-label": "Volume" });
	const full = h("button", { class: "vbtn", type: "button", title: "Full screen (f)" }, "\u2922");
	const box = h("div", { class: "vbox", tabindex: "0" }, player, h("div", { class: "vbar" }, play, seek, clock, sound, volume, full));
	let dragging = false;
	const toggle = () => player.paused || player.ended ? player.play().catch(() => {}) : player.pause();
	const fullscreen = () => {
		const open = document.fullscreenElement || document.webkitFullscreenElement;
		if (open) return void (document.exitFullscreen || document.webkitExitFullscreen).call(document);
		const ask = box.requestFullscreen || box.webkitRequestFullscreen;
		Promise.resolve(ask?.call(box)).catch((e) => toast(`Couldn't go full screen: ${e.message}`));
	};
	const show = () => {
		play.textContent = player.paused || player.ended ? "▶" : "⏸";
		sound.textContent = player.muted || player.volume === 0 ? "\u{1F507}" : "\u{1F50A}";
		clock.textContent = `${time(player.currentTime)} / ${time(player.duration)}`;
		if (!dragging && Number.isFinite(player.duration) && player.duration > 0) seek.value = String(Math.round(player.currentTime / player.duration * 1000));
	};
	play.addEventListener("click", toggle);
	player.addEventListener("click", toggle);
	player.addEventListener("dblclick", fullscreen);
	full.addEventListener("click", fullscreen);
	sound.addEventListener("click", () => { player.muted = !player.muted; if (!player.muted && player.volume === 0) player.volume = volume.value = 1; });
	volume.addEventListener("input", () => { player.volume = Number(volume.value); player.muted = player.volume === 0; });
	seek.addEventListener("input", () => {
		dragging = true;
		if (Number.isFinite(player.duration)) clock.textContent = `${time(seek.value / 1000 * player.duration)} / ${time(player.duration)}`;
	});
	seek.addEventListener("change", () => {
		dragging = false;
		if (Number.isFinite(player.duration)) player.currentTime = seek.value / 1000 * player.duration;
	});
	box.addEventListener("keydown", (e) => {
		if (e.target.closest("input")) return;
		if (e.key === " " || e.key === "k") toggle();
		else if (e.key === "f") fullscreen();
		else if (e.key === "m") player.muted = !player.muted;
		else if (e.key === "ArrowRight") player.currentTime = Math.min(player.duration || 0, player.currentTime + 5);
		else if (e.key === "ArrowLeft") player.currentTime = Math.max(0, player.currentTime - 5);
		else return;
		e.preventDefault();
		e.stopPropagation();
	});
	for (const ev of ["play", "pause", "ended", "timeupdate", "durationchange", "loadedmetadata", "volumechange"]) player.addEventListener(ev, show);
	show();
	return box;
}

// Swaps a player's file for the converted copy, keeping it muted.
async function useConverted(player, status, server, mediaId) {
	status.dataset.converting = "1";
	status.firstChild.textContent = "Making a copy any browser can play\u2026";
	try {
		const got = await playableVideo(server, mediaId, true);
		player.src = got.url;
		player.muted = true;
		status._got = got;
		player.play().catch(() => {});
	} catch (e) {
		toast(`Couldn't make a playable copy: ${e.message}`, 8000);
	}
	delete status.dataset.converting;
}

async function playInline(el, mxc, kind) {
	const [server, mediaId] = mxc.replace("mxc://", "").split("/");
	const button = el.querySelector(".play");
	button.disabled = true;
	button.textContent = "\u2026";
	try {
		let player;
		let note = null;
		if (kind === "video") {
			const got = await playableVideo(server, mediaId);
			const video = videoPlayer(got.url);
			note = playerStatus(video, got, server, mediaId);
			player = videoBox(video);
		} else {
			const got = await playableVideo(server, mediaId);
			player = h(kind, { src: got.url, controls: true, autoplay: true, preload: "metadata" });
			player.muted = player.defaultMuted = true;
		}
		el.classList.add("revealed", "playing");
		el.replaceChildren(player);
		if (note) el.after(note);
	} catch (e) {
		toast(`Couldn't play it: ${e.message}`);
		button.disabled = false;
		button.textContent = "\u25B6";
	}
}

// What the player is doing, in words, under it: which file it got, and
// whether it's moving. Text only, nothing of the video itself, so a video
// that won't play says why.
function playerStatus(player, got, server, mediaId) {
	const text = h("span", {});
	const convert = h("button", { class: "ghost tiny", type: "button", title: "For a video that won't play here: the server makes a copy every browser can play",
		onclick: () => useConverted(player, line, server, mediaId) }, "Won't play? Use a converted copy");
	const line = h("div", { class: "small muted player-status" }, text, " ", convert);
	line._got = got;
	const time = (t) => Number.isFinite(t) ? `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, "0")}` : "?";
	const update = () => {
		if (line.dataset.converting) return;
		const g = line._got;
		convert.hidden = g.converted;
		const source = `${g.converted ? "converted copy" : "the file itself"}, ${g.type || "?"}, ${bytes(g.size)}`;
		const e = player.error;
		const frames = player.getVideoPlaybackQuality?.().totalVideoFrames;
		const state = e ? `error ${e.code}${e.message ? `: ${e.message}` : ""}`
			: player.paused ? (player.readyState < 2 ? "loading" : "paused") : "playing";
		text.textContent = `${source} \u00B7 ${state} \u00B7 ${time(player.currentTime)} / ${time(player.duration)} \u00B7 ${player.videoWidth}\u00D7${player.videoHeight} \u00B7 ${frames ?? "?"} frames`;
	};
	update();
	const timer = setInterval(() => { if (!document.contains(line)) return clearInterval(timer); update(); }, 500);
	for (const ev of ["playing", "pause", "error", "loadeddata", "stalled", "waiting"]) player.addEventListener(ev, update);
	return line;
}

// A video at full size, playing, in a window: what someone sees when they
// open it in an app.
async function openFullVideo(mxc) {
	const [server, mediaId] = mxc.replace("mxc://", "").split("/");
	let got;
	try {
		got = await playableVideo(server, mediaId);
	} catch (e) {
		return toast(`Couldn't play it: ${e.message}`);
	}
	const player = videoPlayer(got.url);
	const shown = ask("Full size", { content: h("div", { class: "full-view" }, videoBox(player), playerStatus(player, got, server, mediaId)), confirm: "Close", cancel: false });
	$("#dialog").dataset.mxc = mxc;
	await shown;
	player.pause();
	delete $("#dialog").dataset.mxc;
	player.removeAttribute("src");
	player.load();
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
	fill(main, 
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
	fill(main, 
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
	fill(main, 
		h("h1", {}, "Notices"),
		h("p", { class: "muted" }, "What the server reports: new users, cleanup runs, plugin failures, reports. Highlighted ones need a look."),
		items.length ? h("div", { class: "notices" }, items) : h("p", { class: "muted" }, "Nothing yet."));
};

// --- AI moderation -------------------------------------------------------------

const MOD = "/_anoncat/admin/v1/moderation";

const serverOf = (userId) => userId.split(":").slice(1).join(":");
const ourServer = () => serverOf(sessionStorage.getItem(USER_KEY) || "");

// Blocks a user or a whole server. Returns true when it went through.
async function blockTarget(target, { reason = "" } = {}) {
	target = target.trim();
	const isUser = target.startsWith("@");
	const local = isUser && serverOf(target) === ourServer();
	const fields = [{ name: "reason", label: "Reason (only admins see it)", value: reason, placeholder: "e.g. spam ads" }];
	if (local) fields.push({ name: "lock", label: "Lock their account, so they can't sign in or post", type: "checkbox", value: true });
	if (isUser) fields.push({ name: "redact", label: local
		? "Remove all their messages everywhere"
		: "Also remove their messages for everyone, in rooms where you're a moderator", type: "checkbox", value: true });
	const v = await ask(`Block ${target}?`, {
		text: isUser
			? "Their messages, old and new, are hidden from everyone on this server, and they can't join rooms through it or invite anyone here. You can unblock them under Blocklist."
			: "Nobody on that server can reach this one: no messages, joins, invites or media, and what they already sent is hidden here. Nothing is sent to them either. You can unblock it under Blocklist.",
		fields, confirm: "Block", danger: true });
	if (!v) return false;
	const done = await attempt(`Blocked ${target}`, () => api("POST", `${MOD}/blocklist`, { target, reason: v.reason, lock: v.lock ?? false }));
	if (done && v.redact) {
		removeMessages(target, v.reason, { blocked: true });
	}
	return Boolean(done);
}

// Removes everything a user said, in every room they were ever in, and swaps
// the files they posted for the stand-in. Runs in the background; says how it
// went when it's done.
async function removeMessages(userId, reason, { blocked = false } = {}) {
	const started = await attempt(null, () => api("POST", `/_synapse/admin/v1/user/${enc(userId)}/redact`,
		{ rooms: [], reason: reason || "Moderation", limit: 100000 }));
	if (!started) return;
	toast("Removing their messages…");
	for (let i = 0; i < 600; i++) {
		await new Promise((done) => setTimeout(done, 2000));
		const s = await api("GET", `/_synapse/admin/v1/user/redact_status/${enc(started.redact_id)}`).catch(() => null);
		if (s?.status === "complete") {
			const left = Object.keys(s.failed_redactions || {}).length;
			return toast(left
				? `Removed ${userId}'s messages, except ${left} in rooms where you aren't a moderator.${blocked ? " Those stay hidden from everyone here." : ""}`
				: `Removed ${userId}'s messages`, 10000);
		}
		if (s?.status === "failed") return toast(`Removing ${userId}'s messages failed${s.error ? `: ${s.error}` : ""}`, 10000);
	}
	toast(`Still removing ${userId}'s messages in the background`);
}

// Blocks a room: nobody here can join it; optionally everyone here leaves.
async function blockRoom(roomId, name) {
	const id = enc(roomId);
	const state = await attempt(null, () => api("GET", `/_synapse/admin/v1/rooms/${id}/block`));
	if (state?.block) {
		if (!(await ask(`Unblock ${name || roomId}?`, { text: "People on this server can join it again.", confirm: "Unblock" }))) return false;
		return Boolean(await attempt("Unblocked", () => api("PUT", `/_synapse/admin/v1/rooms/${id}/block`, { block: false })));
	}
	const v = await ask(`Block ${name || roomId}?`, {
		text: "Nobody on this server can join it, and invites to it are refused.",
		fields: [{ name: "leave", label: "Also make everyone here leave it now", type: "checkbox", value: true }],
		confirm: "Block room", danger: true });
	if (!v) return false;
	if (v.leave) return Boolean(await attempt("Blocked; everyone here is leaving it", () => api("DELETE", `/_synapse/admin/v2/rooms/${id}`, { block: true, purge: false })));
	return Boolean(await attempt("Blocked", () => api("PUT", `/_synapse/admin/v1/rooms/${id}/block`, { block: true })));
}

// Closes the open dialog first: there is only one, and the next action asks.
// It waits for the close event, which would otherwise answer the next one.
const thenAsk = (fn) => () => {
	const dialog = $("#dialog");
	if (!dialog.open) return fn();
	dialog.addEventListener("close", () => setTimeout(fn), { once: true });
	dialog.close();
};

pages.blocklist = async (main) => {
	const [res, rooms] = await Promise.all([
		api("GET", `${MOD}/blocklist`),
		attempt(null, () => command("rooms moderation list-banned-rooms")),
	]);
	const refresh = () => render();
	const target = h("input", { placeholder: "@user:server or server.name", spellcheck: "false" });
	const add = async () => { if (target.value.trim() && await blockTarget(target.value)) refresh(); };
	target.addEventListener("keydown", (e) => e.key === "Enter" && add());

	const rows = res.blocked.map((b) => h("tr", {},
		h("td", {}, h("div", { class: "mono" }, b.target), h("span", { class: "tag" }, b.kind), b.locked ? h("span", { class: "tag bad" }, "account locked") : null),
		h("td", {}, b.reason || h("span", { class: "muted" }, "—")),
		h("td", { class: "small muted" }, `${date(b.ts)}`, h("div", {}, `by ${b.by}`)),
		h("td", { class: "actions" }, h("button", { class: "ghost small", onclick: async () => {
			if (!(await ask(`Unblock ${b.target}?`, { text: b.locked ? "Their account is unlocked too. Messages removed while blocked stay removed." : "Their messages show again, and they can reach this server again.", confirm: "Unblock" }))) return;
			if (await attempt("Unblocked", () => api("DELETE", `${MOD}/blocklist/${enc(b.target)}`))) refresh();
		} }, "Unblock"))));

	const roomIds = [...new Set((rooms?.output || "").match(/^!\S+/gm) || [])];
	const roomRows = roomIds.map((roomId) => h("tr", {},
		h("td", { class: "mono" }, roomId),
		h("td", { class: "actions" },
			h("button", { class: "ghost small", onclick: () => lookAt({ kind: "room", room_id: roomId }, "Blocked room") }, "Look"),
			h("button", { class: "ghost small", onclick: async () => { if (await blockRoom(roomId)) refresh(); } }, "Unblock"))));

	fill(main,
		h("h1", {}, "Blocklist"),
		h("p", { class: "muted" }, "Blocked users' messages are hidden from everyone here, old and new, and they can't join rooms through this server or invite anyone on it. A blocked server can't reach this one at all. Block from a report, a user, or the Look view, or here."),
		h("div", { class: "toolbar" }, target, h("button", { onclick: add }, "Block…")),
		table(["Blocked", "Reason", "When", ""], rows, "Nobody is blocked."),
		h("h2", {}, "Blocked rooms"),
		h("p", { class: "muted small" }, "Nobody here can join these. Block a room from Rooms, a room report, or its Look view."),
		table(["Room", ""], roomRows, "No rooms are blocked."));
};

// Adds a line to the AI's guidance, from a report it got wrong.
async function teachAi(r) {
	const v = await ask("Note for the AI", {
		text: `The AI said: "${r.reason}". Write how cases like this should be judged. What this report was about goes with the note, so the AI can turn "that's fine" into a rule that says what is fine, and keep it to cases like this one. You can change it in Settings.`,
		fields: [{ name: "note", label: "Note", required: true, placeholder: "e.g. Rooms that list what's banned aren't promoting it." }],
		confirm: "Add note" });
	const note = v?.note?.trim();
	if (!note) return false;
	toast("Adding the note; the AI is merging it with the others\u2026", 8000);
	return Boolean(await attempt("Note added and merged with the others", () => api("POST", `${MOD}/notes`, { note, report: r.id })));
}

// Fingerprints of files that must never come in again. Swapping media adds
// its own; lists can be shared between servers' admins, since a hash can't
// be turned back into a picture.
pages.badfiles = async (main) => {
	const BASE = "/_anoncat/admin/v1/badfiles";
	const res = await api("GET", BASE);
	const reload = () => pages.badfiles(main);

	const download = async () => {
		const r = await fetch(`${BASE}/export`, authed()).catch(() => null);
		if (!r?.ok) return toast("Couldn't export the list.");
		const url = URL.createObjectURL(await r.blob());
		const a = h("a", { href: url, download: `anoncat-bad-files-${new Date().toISOString().slice(0, 10)}.csv` });
		document.body.append(a);
		a.click();
		a.remove();
		setTimeout(() => URL.revokeObjectURL(url), 1000);
	};

	const text = h("textarea", { rows: 6, placeholder: "pdq,<64 hex digits>,note\nsha256,<64 hex digits>,note\n<64 hex digits>   (taken as PDQ)", spellcheck: "false" });
	const note = h("input", { placeholder: "Where the list came from (optional)" });
	const file = h("input", { type: "file", accept: ".csv,.txt,text/plain", onchange: async () => {
		const f = file.files?.[0];
		if (f) text.value = await f.text();
	} });
	const doImport = async () => {
		if (!text.value.trim()) return toast("Paste a list or choose a file first.");
		const r = await attempt(null, () => api("POST", `${BASE}/import`, { text: text.value, note: note.value }));
		if (!r) return;
		toast(`${r.added} added${r.skipped ? `, ${r.skipped} already there or not valid` : ""}`);
		if (r.added && (await ask("Check stored media against the list now?", {
			text: "Every file stored here, uploaded or fetched from other servers, is hashed and compared, one at a time in the background. Matches are swapped for the stand-in and reported.",
			confirm: "Scan now" }))) await scan();
		reload();
	};
	const scan = async () => {
		const r = await attempt("Scan started", () => api("POST", `${BASE}/scan`, {}));
		if (r) setTimeout(reload, 2000);
	};

	const sc = res.scan;
	const scanLine = sc.running
		? `Scanning: ${sc.checked} of ${sc.total} checked, ${sc.found} found.`
		: sc.finished_ts ? `Last scan ${date(sc.finished_ts)}: ${sc.checked} checked, ${sc.found} found.` : "No scan run since the server started.";
	if (sc.running) setTimeout(() => { if (location.hash.startsWith("#/badfiles")) reload(); }, 5000);

	const rows = res.entries.map((e) => h("tr", {},
		h("td", {}, h("span", { class: "tag" }, e.kind)),
		h("td", { class: "mono small", title: e.hash }, `${e.hash.slice(0, 16)}…`),
		h("td", { class: "small" }, e.note || h("span", { class: "muted" }, "—")),
		h("td", { class: "small muted" }, `${{ swap: "swapped", import: "imported", admin: "added" }[e.source] || e.source} · ${date(e.added_ts)}`),
		h("td", { class: "actions" }, h("button", { class: "ghost small", onclick: async () => {
			if (!(await ask("Take this off the list?", { text: "Files matching it are let in again. Anything already swapped stays swapped.", confirm: "Remove", danger: true }))) return;
			if (await attempt("Removed", () => api("POST", `${BASE}/remove`, { kind: e.kind, hash: e.hash }))) reload();
		} }, "Remove"))));

	fill(main,
		h("h1", {}, "Known bad files"),
		h("p", { class: "muted" },
			"Fingerprints of files that must never come in again. A matching upload from someone here is refused; a matching file from another server is swapped for the stand-in before anyone sees it, and reported. ",
			"Swapping media adds its fingerprints here by itself."),
		h("p", { class: "muted small" },
			"Two kinds: SHA-256 matches only the very same file; PDQ (Meta's open perceptual hash, the usual format for shared lists) matches the same picture resized, re-encoded or lightly edited. ",
			"A fingerprint is 32 bytes that countless pictures share; no picture can be rebuilt from it, which is why hash lists are what services share instead of files. ",
			"The list is in the encrypted database and its backups; export it too, to keep a copy or give to another server's admin."),
		h("section", { class: "card" },
			h("h2", {}, `${res.count} fingerprints`),
			h("div", { class: "toolbar" },
				h("button", { class: "small", onclick: download, disabled: !res.count }, "Export list"),
				h("button", { class: "ghost small", onclick: scan, disabled: sc.running || !res.count }, "Scan stored media"),
				h("span", { class: "muted small" }, scanLine))),
		h("section", { class: "card" },
			h("h2", {}, "Import"),
			h("p", { class: "muted small" }, "One per line: kind,hash,note. A line with only a hash is taken as PDQ. Lines starting with # are skipped. Entries already on the list are skipped."),
			text,
			h("div", { class: "toolbar" }, file, note, h("button", { class: "small", onclick: doImport }, "Import"))),
		h("h2", {}, res.count > res.entries.length ? `Newest ${res.entries.length}` : "On the list"),
		table(["Kind", "Hash", "Note", "Added", ""], rows, "Nothing yet. Swap a picture, or import a list."));
};

pages.reports = async (main, params) => {
	const all = params.get("all") === "1";
	const res = await api("GET", `${MOD}/reports${all ? "?all=true" : ""}`);
	const refresh = () => render();
	const sevTag = (s) => h("span", { class: `tag ${s === "high" ? "bad" : s === "medium" ? "warn" : ""}` }, s);
	const items = res.reports.map((r) => {
		const act = (action, done) => attempt(done, () => api("POST", `${MOD}/reports/${r.id}`, { action })).then((x) => {
			if (x && action === "hide" && r.mxc) blankMedia(r.mxc, "Hidden");
			return x && refresh();
		});
		// A picture checked as it arrived, uploaded or fetched, has no room or
		// message; its sender is the uploader, or the server it came from.
		const roomless = !r.room_id;
		const fromServer = roomless && r.sender && !r.sender.startsWith("@");
		const buttons = roomless ? [] : [h("button", { class: "small", onclick: () => lookAt(r) }, r.kind === "room" ? "Look at room" : "Look at message")];
		if (r.status === "open") {
			if (r.mxc) buttons.push(r.hidden
				? h("button", { class: "ghost small", onclick: () => act("unhide", "Shown again") }, "Unhide")
				: h("button", { class: r.suggested === "hide" ? "small" : "ghost small", onclick: () => act("hide", "Hidden") }, "Hide media"));
			if (!roomless && r.kind !== "large_file" && r.kind !== "room") buttons.push(h("button", { class: r.suggested === "redact" ? "danger small" : "ghost small", onclick: async () => {
				if (!(await ask("Remove this message?", { text: "Redacts it as you. Needs you to be in the room with the power to remove messages. Other servers drop their copies too.", confirm: "Remove", danger: true }))) return;
				const ok = await attempt("Removed", () => api("PUT", `/_matrix/client/v3/rooms/${enc(r.room_id)}/redact/${enc(r.event_id)}/${Date.now()}`, { reason: "Moderation" }));
				if (ok) await act("resolve", null);
			} }, "Remove message"));
			if (r.kind === "room") {
				buttons.push(h("button", { class: "danger small", onclick: async () => {
					if (await blockRoom(r.room_id, r.room_name)) await act("resolve", null);
				} }, "Block room"));
			} else if (fromServer) {
				buttons.push(h("button", { class: "danger small", onclick: async () => {
					if (await blockTarget(r.sender, { reason: r.category || "" })) refresh();
				} }, `Block ${r.sender}`));
			} else if (r.sender) {
				buttons.push(h("button", { class: "danger small", onclick: async () => {
					if (await blockTarget(r.sender, { reason: r.category || "" })) refresh();
				} }, "Block user"));
				if (serverOf(r.sender) !== ourServer()) buttons.push(h("button", { class: "ghost small", onclick: async () => {
					if (await blockTarget(serverOf(r.sender), { reason: r.category || "" })) refresh();
				} }, `Block ${serverOf(r.sender)}`));
			}
			if (r.kind === "room") buttons.push(h("button", { class: "ghost small", title: "Dismiss, and don't check this room's name and topic again. Its messages are still checked.",
				onclick: () => act("trust_room", "Dismissed; this room's description won't be checked again") }, "Fine, don't check again"));
			buttons.push(h("button", { class: "ghost small", onclick: () => act("dismiss", "Dismissed") }, "Dismiss"));
			buttons.push(h("button", { class: "ghost small", title: "Dismiss, and tell the AI how to judge cases like this from now on",
				onclick: thenAsk(async () => {
					if (await teachAi(r)) await act("dismiss", null);
				}) }, "Dismiss + note for the AI"));
		} else {
			buttons.push(h("button", { class: "ghost small", onclick: () => act("reopen", "Reopened") }, "Reopen"));
			if (r.hidden) buttons.push(h("button", { class: "ghost small", onclick: () => act("unhide", "Shown again") }, "Unhide"));
		}
		let preview = null;
		// The file's own type comes first, so a video gets its frames and
		// can be played here, like in Media.
		if (r.mxc) {
			preview = h("div", {}, h("div", { class: "report-thumb muted small" }, "\u2026"));
			const [server, mediaId] = r.mxc.replace("mxc://", "").split("/");
			api("GET", `/_anoncat/admin/v1/media/item/${enc(server)}/${enc(mediaId)}`)
				.catch(() => null)
				.then((info) => fill(preview,
					mediaThumb(r.mxc, info?.media_type || null, { cls: "report-thumb", held: r.hidden }),
					mediaActions(r.mxc, { info: info || undefined, onChanged: refresh })));
		}
		return h("div", { class: `card report${r.severity === "high" ? " loud" : ""}`, tabindex: "-1",
			"data-group-sender": r.sender || "", "data-group-server": r.sender ? serverOf(r.sender) || r.sender : "", "data-group-room": r.room_name || r.room_id || "" },
			h("div", { class: "report-head" },
				sevTag(r.severity),
				h("span", { class: "tag" }, r.category || r.kind),
				r.hidden ? h("span", { class: "tag warn" }, "hidden") : null,
				r.status !== "open" ? h("span", { class: "tag" }, r.status) : null,
				h("span", { class: "small muted" }, `${date(r.ts)} · ${{ ai: "AI", room: "room check", user: `reported by ${(r.reported_by || []).length || 1}` }[r.source] || "size rule"} · suggests ${r.suggested}`)),
			h("div", {}, r.reason),
			r.reported_by?.length ? h("ul", { class: "reporters small" }, r.reported_by.map((u) =>
				h("li", {}, h("span", { class: "mono" }, u.by), ` · ${date(u.ts)}`, u.reason ? h("span", { class: "muted" }, ` \u2014 ${u.reason}`) : null))) : null,
			preview,
			r.excerpt ? h("pre", { class: "notice-body excerpt" }, r.excerpt) : null,
			h("div", { class: "small muted" }, r.kind === "room"
				? `${r.room_id}${r.sender ? `, joined by ${r.sender}` : ""}`
				: roomless ? `${r.room_name || "Checked as it arrived"}: ${fromServer ? "from" : "by"} ${r.sender}`
				: `${r.sender} in ${r.room_name || r.room_id}`),
			h("div", { class: "toolbar" }, buttons));
	});
	// Grouping: reports from one sender, server or room together, under a
	// heading with their count, biggest group first.
	const groupBy = (() => { try { return sessionStorage.getItem("anoncat-report-group") || ""; } catch { return ""; } })();
	const list = h("div", { class: "notices" });
	const draw = (by) => {
		if (!by) return fill(list, ...items);
		const groups = new Map();
		for (const el of items) {
			const key = el.dataset[`group${by[0].toUpperCase()}${by.slice(1)}`] || "(none)";
			if (!groups.has(key)) groups.set(key, []);
			groups.get(key).push(el);
		}
		const sorted = [...groups.entries()].sort((a, b) => b[1].length - a[1].length);
		fill(list, ...sorted.flatMap(([key, els]) => [h("h3", { class: "group-head" }, `${key} \u00B7 ${els.length}`), ...els]));
	};
	draw(groupBy);
	const groupSelect = h("select", { "aria-label": "Group reports", onchange: () => {
		try { sessionStorage.setItem("anoncat-report-group", groupSelect.value); } catch {}
		draw(groupSelect.value);
	} }, [["", "No grouping"], ["sender", "By sender"], ["server", "By server"], ["room", "By room"]].map(([v, l]) => h("option", { value: v, selected: v === groupBy }, l)));

	// Keys: j and k move between reports; then h hides its media, u unhides,
	// r removes the message, b blocks the sender, o looks at it, d dismisses.
	const KEYS = { h: /^Hide media$/, u: /^Unhide$/, r: /^Remove message$/, b: /^Block (user|room|\S+\.\S+)/, o: /^Look at/, d: /^Dismiss$/ };
	const cards = () => [...list.querySelectorAll(".report")];
	const focused = () => document.activeElement?.closest?.(".report");
	document.removeEventListener("keydown", reportKeys.handler);
	reportKeys.handler = (e) => {
		if (!location.hash.startsWith("#/reports") || e.ctrlKey || e.metaKey || e.altKey) return;
		if (e.target.closest?.("input, textarea, select, dialog")) return;
		const all = cards();
		const at = all.indexOf(focused());
		if (e.key === "j" || e.key === "k") {
			const next = all[Math.max(0, Math.min(all.length - 1, at + (e.key === "j" ? 1 : -1)))] || all[0];
			next?.focus();
			next?.scrollIntoView({ block: "nearest" });
			e.preventDefault();
			return;
		}
		const pattern = KEYS[e.key];
		const card = focused();
		if (!pattern || !card) return;
		const button = [...card.querySelectorAll(".toolbar button")].find((b) => pattern.test(b.textContent.trim()));
		if (button) { e.preventDefault(); button.click(); }
	};
	document.addEventListener("keydown", reportKeys.handler);

	fill(main, 
		h("h1", {}, "Reports"),
		h("p", { class: "muted" }, "What the AI moderation and the size rule flagged. Nothing is removed until you choose to; the AI may only hide images, and only if Settings allow it."),
		h("div", { class: "toolbar" },
			h("button", { class: "ghost small", onclick: () => navigate("reports", all ? {} : { all: "1" }) }, all ? "Show open only" : "Show closed too"),
			items.length > 1 ? groupSelect : null),
		items.length ? h("p", { class: "muted small" }, "Keys: j / k to move, h hide media, u unhide, r remove message, b block, o look, d dismiss.") : null,
		items.length ? list : h("p", { class: "muted" }, all ? "No reports yet." : "Nothing open."));
	items[0]?.focus?.({ preventScroll: true });
};
const reportKeys = {};

// The evidence behind a report: the message with what was said around it, or
// for a room its details and latest messages. Rooms opens it too; nobody has
// to join, but only what this server holds shows.
async function lookAt(r, title) {
	const content = h("div", { class: "evidence" }, h("p", { class: "muted" }, "Loading…"));
	const shown = ask(title || (r.kind === "room" ? "Reported room" : "Reported message"), { content, confirm: "Close", cancel: false });
	try {
		const room = enc(r.room_id);
		const filter = enc(JSON.stringify({ types: SAID_EVENTS }));
		const page = (dir, from) => api("GET", `/_synapse/admin/v1/rooms/${room}/messages?dir=${dir}&limit=40&filter=${filter}${from ? `&from=${enc(from)}` : ""}`);
		const [details, timeline] = await Promise.all([
			api("GET", `/_synapse/admin/v1/rooms/${room}`).catch(() => null),
			r.kind === "room" || !r.event_id
				? page("b").then((m) => ({ events: (m.chunk || []).reverse(), older: m.chunk?.length ? m.end : null }))
				: api("GET", `/_synapse/admin/v1/rooms/${room}/context/${enc(r.event_id)}?limit=20`).then((c) => ({
					events: [...(c.events_before || []).reverse(), c.event, ...(c.events_after || [])].filter(Boolean),
					older: c.start,
					newer: c.end,
				})),
		]);
		const lineOf = (ev) => eventLine(ev, ev.event_id === r.event_id);
		const box = h("div", { class: "timeline" }, timeline.events.filter((ev) => SHOWN_EVENTS.has(ev.type)).map(lineOf));

		// Each click fetches 40 more of what was said; joins and leaves are
		// skipped, so a page may add fewer lines.
		const more = (dir, token) => {
			let from = token;
			const button = h("button", { class: "ghost small", type: "button" }, dir === "b" ? "Load older" : "Load newer");
			const wrap = h("div", { class: "timeline-more" }, button);
			button.onclick = async () => {
				button.disabled = true;
				button.textContent = "Loading…";
				const m = await attempt(null, () => page(dir, from));
				const events = (m?.chunk || []).filter((ev) => SHOWN_EVENTS.has(ev.type));
				if (dir === "b") {
					const before = box.scrollHeight;
					wrap.after(...events.reverse().map(lineOf));
					box.scrollTop += box.scrollHeight - before;
				} else {
					wrap.before(...events.map(lineOf));
				}
				from = m?.end;
				if (!m || !m.chunk?.length || !from) return fill(wrap, h("span", { class: "muted small" }, dir === "b" ? "Nothing older on this server." : "Nothing newer."));
				button.disabled = false;
				button.textContent = dir === "b" ? "Load older" : "Load newer";
			};
			return wrap;
		};
		if (timeline.older) box.prepend(more("b", timeline.older));
		if (timeline.newer) box.append(more("f", timeline.newer));
		const said = box.querySelectorAll(".line").length;

		fill(content,
			details ? roomFacts(details) : h("p", { class: "muted" }, r.room_id),
			h("div", { class: "toolbar" },
				h("button", { class: "ghost small", type: "button", onclick: () => copyText(r.room_id) }, "Copy room ID"),
				h("button", { class: "danger small", type: "button", onclick: thenAsk(() => blockRoom(r.room_id, details?.name)) }, "Block room")),
			details?.encryption ? h("p", { class: "muted small" }, "This room is encrypted: the server only sees that messages were sent, not what they say.") : null,
			said || timeline.older ? box : h("p", { class: "muted" }, r.kind === "room" ? "No messages this server holds, only joins and leaves. It has what was sent while someone here was in the room, not from before." : "No messages this server holds."));
		box.querySelector(".line.reported")?.scrollIntoView({ block: "center" });
		if (!box.querySelector(".line.reported")) box.scrollTop = box.scrollHeight;
	} catch (e) {
		fill(content, h("p", { class: "error" }, `Couldn't load it: ${e.message}`));
	}
	await shown;
}

// Puts text on the clipboard, or shows it to copy by hand where the browser
// won't allow that (the panel opened over plain http from another machine).
async function copyText(text) {
	try {
		await navigator.clipboard.writeText(text);
		toast("Copied");
	} catch {
		const input = h("input", { value: text, readonly: true, onfocus: (e) => e.target.select() });
		const dialog = $("#dialog");
		if (dialog.open) {
			const old = dialog.querySelector(".copy-fallback");
			if (old) old.remove();
			dialog.querySelector("#dialog-body").prepend(h("label", { class: "copy-fallback" }, "Copy this:", input));
			input.focus();
		} else {
			setTimeout(() => input.focus());
			await ask("Copy this", { content: input, confirm: "Done", cancel: false });
		}
	}
}

// What can be done to one media item, wherever it shows: copy its link,
// swap it for the stand-in (hide), delete it, block its server or uploader.
// `info` skips the lookup when the caller already knows it.
function mediaActions(mxc, { info, compact = false, onDeleted, onChanged } = {}) {
	const [server, mediaId] = mxc.replace("mxc://", "").split("/");
	const path = `/_anoncat/admin/v1/media/item/${enc(server)}/${enc(mediaId)}`;
	const cls = compact ? "ghost tiny" : "ghost small";
	const bar = h("div", { class: "toolbar media-actions" });
	const draw = (i) => {
		// Swapping deletes the file for good. Media the AI hid while it waits
		// for review can still be shown again, or swapped to finish it.
		const swap = i.swapped ? null : h("button", { class: cls, type: "button", title: "Deletes the file; everyone, here and on other servers, gets the stand-in picture instead" },
			"Swap for stand-in");
		if (swap) swap.onclick = thenAsk(async () => {
			const text = `${mxc}\nThe file is deleted for good and can't be swapped back. People here, and other servers that ask for it, get the stand-in picture instead.`;
			if (!(await ask("Swap it for the stand-in?", { text, confirm: "Swap and delete", danger: true }))) return;
			const done = await attempt("Swapped; the file is deleted", () => api("POST", `${path}/hide`));
			if (!done) return;
			blankMedia(mxc, "Swapped for the stand-in");
			Object.assign(i, { hidden: true, swapped: true, stored: false });
			draw(i);
			onChanged?.();
		});
		const show = i.hidden && !i.swapped ? h("button", { class: cls, type: "button", title: "Serve the real file again", onclick: async () => {
			if (!(await attempt("Shown again", () => api("DELETE", `${path}/hide`)))) return;
			i.hidden = false;
			draw(i);
			onChanged?.();
		} }, "Show again") : null;
		fill(bar,
			i.swapped ? h("span", { class: "tag warn", title: "The file is deleted; everyone gets the stand-in" }, "swapped")
				: i.hidden ? h("span", { class: "tag warn" }, "hidden") : null,
			h("button", { class: cls, type: "button", title: mxc, onclick: () => copyText(mxc) }, "Copy link"),
			compact ? null : h("button", { class: cls, type: "button", title: "Rooms it was posted in, and who uploaded it", onclick: thenAsk(() => navigate("lookup", { q: mxc })) }, "Where from?"),
			show,
			swap,
			i.stored ? h("button", { class: compact ? "danger tiny" : "danger small", type: "button", onclick: thenAsk(async () => {
				const text = i.local
					? `${mxc}\nUploaded here${i.uploader ? ` by ${i.uploader}` : ""}. It's gone for good, for everyone, including other servers that ask for it.`
					: `${mxc}\nOnly this server's copy goes. It's fetched again if someone here opens it, unless it's swapped or its server is blocked. To get rid of it for good, swap it instead.`;
				if (!(await ask("Delete this media?", { text, confirm: "Delete", danger: true }))) return;
				if (await attempt("Deleted", () => api("DELETE", path))) { blankMedia(mxc, "Deleted"); i.stored = false; draw(i); onDeleted?.(); }
			}) }, "Delete") : null,
			!i.local && !i.server_blocked ? h("button", { class: cls, type: "button", onclick: thenAsk(async () => {
				if (await blockTarget(server)) { i.server_blocked = true; draw(i); onChanged?.(); }
			}) }, `Block ${server}`) : null,
			i.local && i.uploader && i.uploader !== sessionStorage.getItem(USER_KEY) ? h("button", { class: cls, type: "button", onclick: thenAsk(() => blockTarget(i.uploader)) }, "Block uploader") : null);
	};
	if (info) draw({ ...info });
	else api("GET", path).then(draw).catch(() => fill(bar, h("button", { class: cls, type: "button", onclick: () => copyText(mxc) }, "Copy link")));
	return bar;
}

// What people said and did; room setup events would bury it.
const SHOWN_EVENTS = new Set(["m.room.message", "m.room.encrypted", "m.sticker", "m.room.member", "m.room.name", "m.room.topic"]);
// A room's view leaves out joins and leaves, which fill busy rooms.
const SAID_EVENTS = ["m.room.message", "m.room.encrypted", "m.sticker", "m.room.name", "m.room.topic"];

function roomFacts(d) {
	const facts = [
		["Name", d.name || "—"],
		["Topic", d.topic || "—"],
		["Address", d.canonical_alias || "—"],
		["Members", `${d.joined_members ?? "?"} (${d.joined_local_members ?? "?"} from here)`],
		["Public", d.public ? "listed in the directory" : "no"],
		["Encrypted", d.encryption ? "yes" : "no"],
		["Made by", d.creator || "—"],
	];
	return h("dl", { class: "facts" }, facts.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
}

function eventLine(ev, reported) {
	const c = ev.content || {};
	let body;
	let preview = null;
	if (ev.type === "m.room.encrypted") body = h("span", { class: "muted" }, "[encrypted]");
	else if (ev.type === "m.room.message" || ev.type === "m.sticker") {
		if (!c.body && !c.url) body = h("span", { class: "muted" }, "[removed]");
		else if (c.msgtype === "m.image" || ev.type === "m.sticker" || c.msgtype === "m.video" || c.msgtype === "m.file" || c.msgtype === "m.audio") {
			body = `[${(c.msgtype || "m.sticker").slice(2)}] ${c.body || ""}`;
			const isMxc = (u) => typeof u === "string" && u.startsWith("mxc://");
			const file = isMxc(c.url) ? c.url : null;
			const still = isMxc(c.info?.thumbnail_url) ? c.info.thumbnail_url : null;
			const type = c.info?.mimetype || { "m.image": null, "m.video": "video/mp4", "m.audio": "audio/mpeg" }[c.msgtype] || (ev.type === "m.sticker" ? null : "application/octet-stream");
			// The preview is the file itself: the sender's thumbnail_url is
			// theirs to choose, and can show something other than the file.
			const thumb = file && (c.msgtype !== "m.file" || still) ? mediaThumb(file, type) : null;
			preview = thumb || file ? h("div", {}, thumb, file ? mediaActions(file, { compact: true }) : null) : null;
		} else body = c.body;
	} else if (ev.type === "m.room.member") body = h("span", { class: "muted" }, `${c.membership}${ev.state_key !== ev.sender ? ` ${ev.state_key}` : ""}`);
	else if (ev.type === "m.room.name") body = h("span", { class: "muted" }, `named the room "${c.name || ""}"`);
	else if (ev.type === "m.room.topic") body = h("span", { class: "muted" }, `set the topic "${c.topic || ""}"`);
	else body = h("span", { class: "muted" }, ev.type);
	const me = sessionStorage.getItem(USER_KEY);
	return h("div", { class: `line${reported ? " reported" : ""}` },
		h("div", { class: "small muted line-head" }, `${ev.sender} · ${date(ev.origin_server_ts)}`,
			ev.sender !== me ? h("button", { class: "ghost tiny", type: "button", title: `Block ${ev.sender}`, onclick: thenAsk(() => blockTarget(ev.sender)) }, "Block") : null),
		h("div", { class: "line-body" }, body),
		preview);
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
			h("option", { value: "auto" }, vision ? "Auto (the service's default for images)" : "Auto (the service's default)"),
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

	// A listed room by name, with what's known of it here.
	const roomInfo = res.rooms || {};
	const roomRef = (id) => {
		const info = roomInfo[id] || {};
		return h("span", {}, info.name ? h("span", {}, info.name, " ") : null, h("span", { class: "mono small muted" }, id),
			info.here === false ? h("span", { class: "tag warn", title: "Deleted here, or nobody here has joined it yet" }, "not on this server") : null);
	};
	let watched = [...s.extra_rooms];
	const watchedList = h("div");
	const drawWatched = () => fill(watchedList,
		watched.length
			? h("ul", { class: "room-refs" }, watched.map((id) => h("li", {}, roomRef(id), " ",
				h("button", { type: "button", class: "ghost tiny", onclick: () => { watched = watched.filter((w) => w !== id); drawWatched(); } }, "Remove"))))
			: h("p", { class: "muted small" }, "None."));
	drawWatched();
	const extra = h("textarea", { name: "extra_rooms_add", rows: 2, placeholder: "#room:other.server or !roomid:server, one per line", spellcheck: "false" });
	const rules = h("textarea", { name: "rules", class: "rules" }, s.rules);
	const guidance = h("textarea", { name: "guidance", class: "rules" }, s.guidance || "");
	let trusted = [...(s.trusted_rooms || [])];
	const trustedCard = h("section", { class: "card" });
	const drawTrusted = () => fill(trustedCard,
		h("h2", {}, "Rooms whose description is fine"),
		h("p", { class: "muted small" }, "Their name and topic aren't checked again; their messages still are. A room report's \"Fine, don't check again\" adds it here."),
		trusted.length
			? h("ul", { class: "room-refs" }, trusted.map((id) => h("li", {}, roomRef(id), " ",
				h("button", { type: "button", class: "ghost tiny", onclick: () => { trusted = trusted.filter((t) => t !== id); drawTrusted(); } }, "Remove"))))
			: h("p", { class: "muted small" }, "None yet."));
	drawTrusted();
	const field = (label, input, help) => h("label", { class: "field" }, h("span", {}, label), input, help ? h("span", { class: "muted small" }, help) : null);
	const batch = h("input", { type: "number", name: "batch_minutes", min: 1, max: 60, value: s.batch_minutes });
	const large = h("input", { type: "number", name: "large_file_mb", min: 0, value: s.large_file_mb });
	const testOut = h("p", { class: "muted small" });

	const form = h("form", { class: "settings" },
		h("section", { class: "card" },
			h("h2", {}, "AI moderation"),
			res.key_saved
				? h("p", { class: "small" }, h("span", { class: "tag ok" }, "key saved"), "AI service key found.")
				: h("p", { class: "small" }, h("span", { class: "tag warn" }, "no key"), `Save your AI service key in ${res.key_file || "the file set by ai_key_file"} first; the AI does nothing without it.`),
			check("enabled", "Turn on AI moderation", "Only rooms that aren't encrypted are ever read. Names and IDs are replaced with placeholders (U1, R1) before anything is sent to the AI service."),
			h("div", { class: "toolbar" }, h("button", { type: "button", class: "ghost small", onclick: async () => {
				testOut.textContent = "Testing…";
				const r = await attempt(null, () => api("POST", `${MOD}/test`, {}));
				testOut.textContent = r ? r.message : "Test failed.";
			} }, "Test the AI connection")),
			testOut),
		h("section", { class: "card" },
			h("h2", {}, "Which rooms"),
			check("watch_directory", "Rooms in this server's public room list"),
			check("watch_public_joined", "Any room anyone can join that someone here is in", "Includes public rooms on other servers that people here joined."),
			h("h3", {}, "Extra rooms"),
			h("p", { class: "muted small" }, "Their messages are checked too. A room the AI flags is added here, so its new messages get checked; it leaves the list when you dismiss its report or delete the room."),
			watchedList,
			field("Add rooms", extra, "Rooms from anywhere in the public room space. The server only sees a room once someone here is in it, so join it with your account first."),
			check("check_rooms", "Check rooms people here join", "When someone here joins a room, public or not, or a room changes its name or topic, the AI looks at its name, topic and latest messages. A room that looks wrong becomes a report and a notice, and is watched from then on."),
			h("div", { class: "toolbar" }, h("button", { type: "button", class: "ghost small", onclick: async () => {
				const r = await attempt(null, () => api("POST", `${MOD}/rooms/check`, {}));
				if (r) toast(r.message);
			} }, "Check all rooms now"))),
		h("section", { class: "card" },
			h("h2", {}, "What to check"),
			check("check_messages", "Messages"),
			check("check_images", "Images (up to 8 MiB)", "Long pictures are also checked in parts, and animated ones frame by frame, so nothing hides past the first glance."),
			check("check_uploads", "Also check every picture uploaded here", "Not only ones posted in watched rooms: a picture sent in an encrypted room, or only shared as a link, is otherwise never checked. Each one goes to the AI service the same way as images in public rooms. Encrypted attachments can't be read and are skipped."),
			check("check_remote", "Also check every picture fetched from other servers", "Each picture someone here opens from another server is checked once, as it arrives, wherever it was posted. This is what covers the hundreds of servers you can't go through by hand. Only full pictures are fetched this way; small previews and profile pictures usually come as thumbnails and aren't. Costs one AI check per picture."),
			field("Report files at least this big (MiB, 0 = off)", large, "Large files get a report for you to look at, without the AI."),
			field("Check new messages every (minutes)", batch, "Messages are checked together, which is cheaper. 40 waiting messages are checked at once.")),
		h("section", { class: "card" },
			h("h2", {}, "Images the AI judges clearly bad"),
			mode),
		standInCard(s),
		h("section", { class: "card" },
			h("h2", {}, "Uploads"),
			check("upload_guard", "Don't let this server be used as file storage", "Refuses pieces of split-up files (chunk_0.enc and so on), and uploads from an account that has uploaded more files today than it posted messages, plus the allowance below. Real attachments are always posted, encrypted rooms included. Admins aren't limited. See Media, Storage check."),
			field("Extra uploads a day", h("input", { type: "number", name: "upload_allowance", min: 1, max: 10000, value: s.upload_allowance ?? 100 }), "Files an account may upload in a day beyond the messages it posted. Apps upload every file of a message before posting it, so this must cover the biggest batch of pictures someone sends at once. Room and profile pictures count here.")),
		h("section", { class: "card" },
			h("h2", {}, "Models and rules"),
			field("Text model", modelSelect("text_model", false)),
			field("Image model", modelSelect("vision_model", true), "The open models (Qwen, DeepSeek, GLM, Gemma, Llama) run on the no-retention service itself, which keeps nothing. Gemini, Grok, Claude and GPT get passed on to their own companies; avoid them for privacy."),
			field("Server rules, as the AI is told them", rules),
			field("Notes on judging", guidance, "How to judge borderline cases, in your words. Added to every check, and the AI follows them over its own sense. \"Dismiss + note for the AI\" on a report adds a note here, and the AI merges it with the others so the list stays short."),
			h("div", { class: "toolbar" },
				h("button", { type: "button", class: "ghost small", onclick: async () => {
					toast("Tidying the notes\u2026", 8000);
					const r = await attempt("Notes tidied", () => api("POST", `${MOD}/notes`, { action: "tidy" }));
					if (r) guidance.value = r.guidance;
				} }, "Tidy notes with the AI"),
				h("button", { type: "button", class: "ghost small", title: "Put back the notes from before the last tidy", onclick: async () => {
					const r = await attempt("Earlier notes put back", () => api("POST", `${MOD}/notes`, { action: "undo" }));
					if (r) guidance.value = r.guidance;
				} }, "Undo last tidy"))),
		trustedCard,
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
			check_uploads: f.has("check_uploads"),
			check_remote: f.has("check_remote"),
			check_rooms: f.has("check_rooms"),
			media_mode: f.get("media_mode") || "suggest",
			extra_rooms: [...watched, ...String(f.get("extra_rooms_add") || "").split(/\s+/).filter(Boolean)],
			large_file_mb: Number(f.get("large_file_mb") || 0),
			batch_minutes: Number(f.get("batch_minutes") || 2),
			text_model: f.get("text_model"),
			vision_model: f.get("vision_model"),
			rules: f.get("rules"),
			cat_stand_in: f.has("cat_stand_in"),
			stand_in_image: f.get("stand_in_image") || "orange",
			guidance: f.get("guidance") || "",
			trusted_rooms: trusted,
			upload_guard: f.has("upload_guard"),
			upload_allowance: Number(f.get("upload_allowance") || 100),
		};
		if (await attempt("Saved", () => api("PUT", `${MOD}/settings`, body))) render();
	});

	// One form, shown a tab at a time; every field stays in it, so saving
	// sends them all.
	const TABS = [
		["general", "General", ["AI moderation", "Which rooms"]],
		["checks", "What to check", ["What to check", "Images the AI judges clearly bad", "Uploads"]],
		["standin", "Stand-in picture", ["Hidden and blocked media"]],
		["ai", "Models and rules", ["Models and rules"]],
		["trusted", "Trusted rooms", []],
	];
	const tabOf = (section) => {
		if (section === trustedCard) return "trusted";
		const title = section.querySelector("h2")?.textContent || "";
		return (TABS.find(([, , titles]) => titles.includes(title)) || TABS[0])[0];
	};
	const sections = [...form.children].filter((el) => el.tagName === "SECTION");
	let current = (() => { try { return sessionStorage.getItem("anoncat-settings-tab"); } catch { return null; } })() || "general";
	const bar = h("div", { class: "tabs", role: "tablist" });
	const show = (tab) => {
		current = tab;
		try { sessionStorage.setItem("anoncat-settings-tab", tab); } catch {}
		for (const section of sections) section.hidden = tabOf(section) !== tab;
		for (const b of bar.children) b.setAttribute("aria-selected", String(b.dataset.tab === tab));
	};
	for (const [id, label] of TABS) bar.append(h("button", { type: "button", role: "tab", "data-tab": id, class: "tab", onclick: () => show(id) }, label));
	show(TABS.some(([id]) => id === current) ? current : "general");
	fill(main, h("h1", {}, "Settings"), bar, form);
};

// What people here see in place of hidden media and media from blocked
// servers: a built-in picture or the admin's own.
function standInCard(s) {
	const BASE = "/_anoncat/admin/v1/media/stand-in";
	const choices = [["orange", "Orange cat"], ["grey", "Grey cat"], ["black", "Black cat"], ["plain", "Plain"], ["custom", "Your own"]];
	const picture = (name) => {
		const box = h("div", { class: "stand-in-pic" }, h("span", { class: "muted small" }, name === "custom" ? "none uploaded" : ""));
		(async () => {
			try {
				const res = await fetch(`${BASE}/${name}`, { headers: { Authorization: `Bearer ${sessionStorage.getItem(TOKEN_KEY)}` }, credentials: "omit", cache: "no-store" });
				if (!res.ok) return;
				const blob = await res.blob();
				if (!RASTER.has(blob.type)) return;
				const url = URL.createObjectURL(blob);
				box.replaceChildren(h("img", { src: url, alt: "", onload: () => URL.revokeObjectURL(url) }));
			} catch { /* leave the placeholder */ }
		})();
		return box;
	};
	const radios = h("div", { class: "stand-in-choices" });
	const drawChoices = () => fill(radios, choices.map(([name, label]) => h("label", { class: "stand-in-choice" },
		h("input", { type: "radio", name: "stand_in_image", value: name, checked: (s.stand_in_image || "orange") === name }),
		picture(name),
		h("span", { class: "small" }, label))));
	drawChoices();

	const file = h("input", { type: "file", accept: "image/png,image/jpeg,image/webp", class: "hidden-input", onchange: async () => {
		const f = file.files[0];
		file.value = "";
		if (!f) return;
		const done = await attempt("Picture saved, without its metadata", async () => {
			const res = await fetch(BASE, { method: "PUT", headers: { Authorization: `Bearer ${sessionStorage.getItem(TOKEN_KEY)}`, "Content-Type": "application/octet-stream" },
				body: f, credentials: "omit", cache: "no-store" });
			const json = await res.json().catch(() => ({}));
			if (!res.ok) throw new ApiError(res.status, json);
			return json;
		});
		if (done) { s.stand_in_image = "custom"; drawChoices(); }
	} });

	return h("section", { class: "card" },
		h("h2", {}, "Hidden and blocked media"),
		h("label", { class: "check" }, h("input", { type: "checkbox", name: "cat_stand_in", checked: s.cat_stand_in !== false }),
			h("span", {}, "Show a stand-in picture instead", h("span", { class: "muted small block" },
				"People here see this picture in place of media you or the AI hid and media from servers you blocked, instead of a broken image. It gets a random file name each time and carries no metadata."))),
		radios,
		h("div", { class: "toolbar" },
			h("button", { type: "button", class: "ghost small", onclick: () => file.click() }, "Upload your own…"),
			h("button", { type: "button", class: "ghost small", onclick: async () => {
				if (await attempt("Removed", () => api("DELETE", BASE))) {
					if (s.stand_in_image === "custom") s.stand_in_image = "orange";
					drawChoices();
				}
			} }, "Remove yours"),
			h("span", { class: "muted small" }, "PNG, JPEG or WebP up to 2 MiB. Its metadata (location, camera, dates) is removed when it's saved. Click Save settings to switch to it.")),
		file);
}

// The assistant's chat lives only in this tab.
const chat = [];
let redrawChat = () => {};

pages.assistant = async (main) => {
	const log = h("div", { class: "chat" });
	const input = h("textarea", { placeholder: "Ask about the server, e.g. \"how busy is it?\" or \"how do I stop registrations?\"" });
	const draw = () => {
		log.replaceChildren(...chat.map((t) => t.shared
			? h("div", { class: "card bubble shared" },
				h("div", { class: "small muted" }, t.auto ? `Ran ${t.command} by itself` : `Output of ${t.command}, shared with the assistant`),
				h("pre", { class: "notice-body excerpt" }, t.output))
			: h("div", { class: `card bubble ${t.role}` }, renderReply(t.content, t.role === "assistant" ? (t.ran ? "ran" : "run") : null))));
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
		log.append(h("p", { class: "muted small" }, "Thinking and looking things up…"));
		const messages = chat.filter((t) => !t.failed).map(({ role, content }) => ({ role, content }));
		let r = null;
		try { r = await api("POST", `${MOD}/assistant`, { messages }); }
		catch (e) { chat.push({ role: "assistant", failed: true, content: `(no answer: ${e.message})` }); }
		for (const t of r ? r.turns : []) chat.push(t.command
			? { role: "user", shared: true, auto: true, command: t.command, output: t.output, content: t.content }
			: { role: t.role, content: t.content, ran: t.ran });
		draw();
	};
	redrawChat = draw;
	// Enter sends; Shift+Enter makes a new line.
	input.addEventListener("keydown", (e) => {
		if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); }
	});
	fill(main, 
		h("h1", {}, "Assistant"),
		h("p", { class: "muted" }, "Chats with the no-retention AI service. It sees a snapshot of the server's state each time you send, with people and rooms replaced by placeholders. Commands that only look things up it runs by itself and reads their output; anything that changes something only runs when you click Run and confirm. Ctrl+Enter sends."),
		log, input,
		h("div", { class: "toolbar" },
			h("button", { onclick: send }, "Send"),
			h("button", { class: "ghost", onclick: () => { chat.length = 0; draw(); } }, "Clear")));
	draw();
};

// Characters of a command's output shared with the assistant.
const SHARED_OUTPUT = 6000;

// Plain text, with ```command blocks turned into commands to run on request
// ("run"), or shown as already run by the assistant itself ("ran").
function renderReply(text, commands) {
	const parts = [];
	const re = /```command\s*\n([\s\S]*?)```/g;
	let last = 0, m;
	while (commands && (m = re.exec(text))) {
		if (m.index > last) parts.push(h("pre", { class: "notice-body" }, text.slice(last, m.index).trim()));
		const cmd = m[1].trim().replace(/^!admin\s+/, "");
		if (commands === "ran") {
			parts.push(h("div", { class: "proposed" }, h("pre", { class: "mono" }, cmd), h("span", { class: "tag ok" }, "ran by itself")));
			last = re.lastIndex;
			continue;
		}
		parts.push(h("div", { class: "proposed" },
			h("pre", { class: "mono" }, cmd),
			h("button", { class: "small", onclick: async () => {
				// Shown exactly as it will run, line breaks and all: the AI
				// read room names and messages others wrote.
				const lines = cmd.split("\n").length;
				const text = lines > 1
					? `This is ${lines} lines: the first is the command, the rest is its input. Read all of it.`
					: "Check it does what you want. The AI's suggestions can be wrong.";
				if (!(await ask("Run this command?", { text, pre: cmd, confirm: "Run", danger: true }))) return;
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

// --- plugins -------------------------------------------------------------------

const PLUGINS = "/_anoncat/admin/v1/plugins";

function capabilityTags(c) {
	if (!c) return [];
	const tags = [];
	if (c.check_events) tags.push(h("span", { class: "tag warn", title: "May refuse local users' messages" }, "check events"));
	if (c.check_fail_closed) tags.push(h("span", { class: "tag bad", title: "Refuses messages when its check fails or times out" }, "fail closed"));
	if (c.send_messages) tags.push(h("span", { class: "tag warn", title: "May send messages as the server" }, "send messages"));
	if (c.events.length) tags.push(h("span", { class: "tag", title: "Sees these events after they're stored" }, `events: ${c.events.join(", ")}`));
	return tags.length ? tags : [h("span", { class: "muted small" }, "no capabilities")];
}

pages.plugins = async (main) => {
	let res = await api("GET", PLUGINS);
	const draw = () => {
		const items = res.plugins.map((p) => {
			const act = async (action, done, confirmText) => {
				if (confirmText && !(await ask(confirmText.title, { text: confirmText.text, confirm: confirmText.confirm, danger: confirmText.danger }))) return;
				const r = await attempt(done, () => api("POST", `${PLUGINS}/${enc(p.name)}`, { action }));
				if (r) { res = r; draw(); }
			};
			const buttons = [];
			if (p.capabilities && !p.approved) buttons.push(h("button", { class: "small", onclick: () => act("approve", "Approved", {
				title: `Approve ${p.name}?`,
				text: `It may then: ${describeCaps(p.capabilities)}. The approval covers these capabilities; replacing the plugin's code keeps it, asking for more needs a new one.`,
				confirm: "Approve",
			}) }, "Approve"));
			if (p.loaded) {
				buttons.push(h("button", { class: "ghost small", onclick: () => act("reload", "Reloaded") }, "Reload"));
				buttons.push(h("button", { class: "ghost small", onclick: () => act("unload", "Unloaded") }, "Unload"));
			} else if (p.capabilities && p.approved) {
				buttons.push(h("button", { class: "small", onclick: () => act("load", "Loaded") }, "Load"));
			}
			if (p.approval) buttons.push(h("button", { class: "danger small", onclick: () => act("revoke", "Approval withdrawn", {
				title: `Withdraw ${p.name}'s approval?`, text: "It's unloaded now and won't load again until approved.", confirm: "Withdraw", danger: true,
			}) }, "Withdraw approval"));
			if (p.alerts.length) buttons.push(h("button", { class: "ghost small", onclick: () => act("clear_alerts", null) }, "Clear problems"));

			return h("div", { class: `card report${p.alerts.length ? " loud" : ""}` },
				h("div", { class: "report-head" },
					h("strong", {}, p.name),
					p.loaded ? h("span", { class: "tag ok" }, "loaded") : h("span", { class: "tag" }, "not loaded"),
					p.enabled ? h("span", { class: "tag" }, "starts with the server") : null,
					p.capabilities ? (p.approved ? h("span", { class: "tag ok" }, "approved") : h("span", { class: "tag warn" }, "needs approval")) : null),
				p.description ? h("div", {}, p.description) : null,
				p.manifest_error ? h("p", { class: "error" }, p.manifest_error) : null,
				h("div", { class: "toolbar" }, capabilityTags(p.capabilities)),
				p.approval ? h("div", { class: "small muted" }, `Approved by ${p.approval.by} on ${date(p.approval.ts)}${p.approved ? "" : ` for less than it asks now: ${describeCaps(p.approval.capabilities)}`}`) : null,
				p.alerts.length ? h("div", {},
					h("div", { class: "small" }, "Problems:"),
					h("pre", { class: "notice-body excerpt" }, p.alerts.slice().reverse().map((a) => `${date(a.ts)}  ${a.message}`).join("\n"))) : null,
				h("div", { class: "toolbar" }, buttons));
		});
		fill(main, 
			h("h1", {}, "Plugins"),
			h("p", { class: "muted" }, "WebAssembly plugins from ", h("span", { class: "mono" }, res.dir), ": put ", h("span", { class: "mono" }, "<name>.wasm"), " and its manifest ", h("span", { class: "mono" }, "<name>.toml"), " there. A plugin loads only once you approve the capabilities its manifest declares. A plugin that keeps failing (a time or memory limit) is unloaded and shows here."),
			h("div", { class: "toolbar" }, h("button", { class: "ghost small", onclick: () => render() }, "Refresh")),
			items.length ? h("div", { class: "notices" }, items) : h("p", { class: "muted" }, "No plugins in the directory."));
	};
	draw();
};

function describeCaps(c) {
	const out = [];
	if (c.check_events) out.push("refuse local users' messages");
	if (c.check_fail_closed) out.push("refuse every message while its check fails");
	if (c.send_messages) out.push("send messages as the server");
	if (c.events.length) out.push(`read stored events of type ${c.events.join(", ")}`);
	return out.join("; ") || "nothing";
}

// --- cleanup -------------------------------------------------------------------

const CLEANUP = "/_anoncat/admin/v1/cleanup";

// [key in the request, key in limits, label, kind]
const CLEANUP_FIELDS = [
	["interval", "interval_secs", "Runs every", "duration"],
	["remote_media_max_age", "remote_media_max_age_secs", "Remote media kept for", "duration"],
	["local_media_max_age", "local_media_max_age_secs", "Local uploads kept for", "duration"],
	["local_media_max_size", "local_media_max_size", "Local uploads size cap", "size"],
	["message_max_age", "message_max_age_secs", "Message history kept for", "duration"],
	["media_message_max_age", "media_message_max_age_secs", "Messages with media kept for", "duration"],
	["disk_percent", "disk_percent", "Disk guard at (% used)", "percent"],
	["min_free_ram", "min_free_ram", "Low-memory guard below", "size"],
];

function showLimit(kind, value) {
	if (kind === "duration") return duration(value);
	if (kind === "size") return value ? bytes(value) : "off";
	return value ? `${value}%` : "off";
}

// A value as the config file would write it, for editing.
function editLimit(kind, value) {
	if (value === null || value === undefined) return "";
	if (!value) return "0";
	if (kind === "duration") {
		for (const [unit, secs] of [["d", 86400], ["h", 3600], ["m", 60]]) if (value % secs === 0) return `${value / secs}${unit}`;
		return `${value}s`;
	}
	if (kind === "size") {
		for (const [unit, n] of [["GiB", 1 << 30], ["MiB", 1 << 20], ["KiB", 1 << 10]]) if (value % n === 0) return `${value / n} ${unit}`;
		return String(value);
	}
	return String(value);
}

pages.cleanup = async (main) => {
	const res = await api("GET", CLEANUP);
	const inputs = {};
	const rows = CLEANUP_FIELDS.map(([key, limit, label, kind]) => {
		const override = res.overrides[limit];
		const input = h("input", { name: key, value: editLimit(kind, override), placeholder: `config: ${editLimit(kind, res.configured[limit])}` });
		inputs[key] = input;
		return h("tr", {},
			h("td", {}, label),
			h("td", {}, showLimit(kind, res.limits[limit]), override !== null && override !== undefined ? h("span", { class: "tag warn" }, "panel") : null),
			h("td", { class: "small muted" }, showLimit(kind, res.configured[limit])),
			h("td", {}, input));
	});

	const save = async () => {
		const body = {};
		for (const [key] of CLEANUP_FIELDS) body[key] = inputs[key].value.trim() || null;
		if (await attempt("Saved", () => api("PUT", CLEANUP, body))) render();
	};
	const reset = async () => {
		const body = Object.fromEntries(CLEANUP_FIELDS.map(([key]) => [key, null]));
		if (await attempt("Back to the config file", () => api("PUT", CLEANUP, body))) render();
	};
	const runNow = async () => {
		if (!(await ask("Run cleanup now?", { text: "Deletes whatever the limits in force say should go: expired media and, if set, old message history. This can't be undone.", confirm: "Run cleanup", danger: true }))) return;
		if (await attempt("Cleanup started", () => api("POST", `${CLEANUP}/run`, {}))) render();
	};

	const history = res.history.map((r) => {
		const rep = r.report;
		const parts = [];
		if (rep.remote_media) parts.push(`${rep.remote_media} remote media`);
		if (rep.local_media_expired) parts.push(`${rep.local_media_expired} expired uploads`);
		if (rep.local_media_over_limit) parts.push(`${rep.local_media_over_limit} uploads over the cap (${rep.local_media_freed_mib} MiB)`);
		if (rep.events_purged) parts.push(`${rep.events_purged} events from ${rep.rooms_purged} rooms`);
		if (rep.disk_guard_media) parts.push(`${rep.disk_guard_media} media for the disk guard`);
		return h("tr", {},
			h("td", { class: "small" }, date(r.ts)),
			h("td", { class: "small muted" }, r.trigger),
			h("td", { class: "small muted" }, `${(r.millis / 1000).toFixed(1)} s`),
			h("td", {}, r.error ? h("span", { class: "error" }, r.error) : (parts.join(", ") || h("span", { class: "muted" }, "nothing to delete")),
				rep.disk_percent !== null && rep.disk_percent !== undefined ? h("span", { class: "muted small" }, ` · disk ${rep.disk_percent}%`) : null));
	});

	fill(main, 
		h("h1", {}, "Cleanup"),
		h("p", { class: "muted" }, "Limits for the scheduled cleanup. A value set here replaces the config file's cleanup_* option until you clear it; leave a box empty to use the config file. Durations like 30d, 12h; sizes like 50 GiB; 0 turns a rule off."),
		table(["Rule", "In force", "Config file", "Set here"], rows),
		h("div", { class: "toolbar" },
			h("button", { onclick: save }, "Save"),
			h("button", { class: "ghost", onclick: reset }, "Use the config file for all"),
			h("div", { class: "grow" }),
			res.running ? h("span", { class: "tag warn" }, "running…") : h("button", { class: "danger", onclick: runNow }, "Run cleanup now")),
		h("h2", {}, "Last runs"),
		table(["Started", "By", "Took", "Deleted"], history, "No runs yet."));

	// Follow a run until it finishes.
	if (res.running) setTimeout(() => location.hash.startsWith("#/cleanup") && render(), 3000);
};

// --- backups -------------------------------------------------------------------

const BACKUPS = "/_anoncat/admin/v1/backups";

pages.backups = async (main) => {
	const s = await api("GET", BACKUPS);
	const newest = s.backups[s.backups.length - 1];
	const ageHours = newest ? (Date.now() - newest.ts) / 3600000 : null;
	const verifyOut = h("p", { class: "muted small" });

	const rows = s.backups.slice().reverse().map((b) => h("tr", {},
		h("td", { class: "mono" }, `#${b.id}`),
		h("td", {}, date(b.ts)),
		h("td", { class: "small muted" }, `${bytes(b.added)} new · ${b.files} files`),
		h("td", { class: "actions" }, h("button", { class: "ghost small", onclick: async (e) => {
			e.target.disabled = true;
			verifyOut.textContent = `Checking #${b.id}…`;
			const r = await attempt(null, () => api("POST", `${BACKUPS}/${b.id}/verify`, {}));
			e.target.disabled = false;
			verifyOut.textContent = r ? r.message : "Check failed.";
		} }, "Verify"))));

	const last = s.last;
	fill(main, ...[
		h("h1", {}, "Backups"),
		!s.path
			? h("p", { class: "error" }, "Backups are off: set database_backup_path (inside the vault) and database_backups_to_keep in the config.")
			: h("div", { class: "grid" },
				stat("Newest backup", newest ? duration(Math.max(60, Math.round((Date.now() - newest.ts) / 1000))) + " ago" : "none", newest ? date(newest.ts) : "make one below", null),
				stat("Stored", `${s.backups.length} of ${s.keep}`, s.size_on_disk !== null && s.size_on_disk !== undefined ? `${bytes(s.size_on_disk)} on disk` : ""),
				stat("Free in the vault", bytes(s.disk_available), s.disk_total ? `of ${bytes(s.disk_total)}` : "", s.disk_total ? meter(1 - s.disk_available / s.disk_total) : null)),
		ageHours !== null && ageHours > 48 ? h("p", { class: "error" }, "The newest backup is over two days old. Check the anoncat-backup timer (systemctl status anoncat-backup.timer).") : null,
		s.error && s.path ? h("p", { class: "error" }, s.error) : null,
		last ? h("p", { class: last.error ? "error" : "muted small" }, last.error
			? `Last backup since the server started failed (${date(last.ts)}, ${last.trigger}): ${last.error}`
			: `Last backup since the server started: ${date(last.ts)}, by ${last.trigger}, took ${(last.millis / 1000).toFixed(1)} s.`) : null,
		h("div", { class: "toolbar" },
			s.running ? h("span", { class: "tag warn" }, "backing up…") : h("button", { disabled: !s.path, onclick: async () => {
				if (await attempt("Backup started", () => api("POST", BACKUPS, {}))) render();
			} }, "Back up now")),
		h("p", { class: "muted small" }, s.path ? `Stored in ${s.path}. Unchanged files are shared between backups, so each one adds only what changed.` : ""),
		table(["Backup", "Made", "Size", ""], rows, "No backups yet."),
		verifyOut,
		h("h2", {}, "Off this machine"),
		h("p", { class: "muted" }, "These backups live in the vault, on the same drive as the database: they undo mistakes and corruption, not a dead drive or a stolen PC. Media files aren't in them. Also keep a LUKS header backup somewhere else (without it a damaged header loses everything): as root, run anoncat-backup header /path/off/this/machine. See ops/backup/README.md for restoring."),
	].filter(Boolean));

	if (s.running) setTimeout(() => location.hash.startsWith("#/backups") && render(), 3000);
};

// --- logs ----------------------------------------------------------------------

pages.logs = async (main, params) => {
	const level = params.get("level") || "info";
	const q = (params.get("q") || "").toLowerCase();
	const res = await api("GET", `/_anoncat/admin/v1/logs?level=${enc(level)}&limit=2000`);
	const lines = res.lines.filter((l) => !q || l.message.toLowerCase().includes(q) || l.target.toLowerCase().includes(q));
	const text = lines.slice().reverse().map((l) => `${new Date(l.ts).toLocaleTimeString()} ${l.level.padEnd(5)} ${l.target.replace(/^tuwunel_/, "")}: ${l.message}`).join("\n");
	const go = (changes) => navigate("logs", { level, q, ...changes });

	fill(main, 
		h("h1", {}, "Logs"),
		h("p", { class: "muted" }, "The server's recent log lines, newest first, kept in memory only (the last 2,000 since it started). IP addresses are blanked."),
		h("div", { class: "toolbar" },
			h("select", { onchange: (e) => go({ level: e.target.value }) },
				...[["info", "Info and worse"], ["warn", "Warnings and errors"], ["error", "Errors only"]].map(([v, label]) => {
					const o = h("option", { value: v }, label);
					o.selected = v === level;
					return o;
				})),
			h("input", { placeholder: "Filter", value: params.get("q") || "", onkeydown: (e) => e.key === "Enter" && go({ q: e.target.value }) }),
			h("button", { class: "ghost small", onclick: () => render() }, "Refresh")),
		lines.length ? h("pre", { class: "output logs" }, text) : h("p", { class: "muted" }, "Nothing logged at this level yet."));
};

// Links and IDs pasted from a client: mxc:// links, matrix.to and matrix:
// links, media download URLs, and bare user, room, alias or server names.
// Several can be pasted at once, one per line or separated by spaces.
function parseLinks(text) {
	const out = [];
	for (let raw of text.split(/\s+/).filter(Boolean).slice(0, 25)) {
		raw = raw.replace(/^[<("']+|[>)"',.]+$/g, "");
		let id = raw;
		let event = null;
		try {
			const url = new URL(raw);
			if (url.protocol === "mxc:") id = raw;
			else if (url.hostname === "matrix.to") {
				const [first, second] = url.hash.replace(/^#\/?/, "").split("?")[0].split("/").map(decodeURIComponent);
				id = first;
				if (second?.startsWith("$")) event = second;
			} else if (url.protocol === "matrix:") {
				const parts = url.pathname.split("?")[0].split("/").map(decodeURIComponent);
				const sigil = { u: "@", r: "#", roomid: "!" }[parts[0]];
				if (sigil) id = sigil + parts[1];
				if (parts[2] === "e" && parts[3]) event = `$${parts[3]}`;
			} else {
				const m = url.pathname.match(/\/_matrix\/(?:client\/v1|media\/v3|media\/r0)\/media?\/?(?:download|thumbnail)\/([^/]+)\/([^/?#]+)/)
					|| url.pathname.match(/\/(?:download|thumbnail)\/([^/]+)\/([^/?#]+)/);
				if (m) id = `mxc://${decodeURIComponent(m[1])}/${decodeURIComponent(m[2])}`;
			}
		} catch { /* not a URL: a bare ID */ }
		let kind = null;
		if (/^mxc:\/\/[^/\s]+\/[A-Za-z0-9_-]+$/.test(id)) kind = "media";
		else if (/^@[^:\s]+:\S+$/.test(id)) kind = "user";
		else if (/^![^\s]+$/.test(id)) kind = "room";
		else if (/^#[^:\s]+:\S+$/.test(id)) kind = "alias";
		else if (/^\$\S+$/.test(id)) kind = "event";
		else if (/^[a-z0-9.-]+\.[a-z]{2,}(:\d+)?$/i.test(id)) kind = "server";
		out.push({ raw, id, kind, event });
	}
	return out;
}

pages.lookup = async (main, params) => {
	const q = params.get("q") || "";
	const input = h("textarea", { class: "lookup-box", placeholder: "Paste links or IDs from your client: mxc://…, https://matrix.to/#/…, @user:server, !room, #alias:server, or a server name", spellcheck: "false" }, q);
	const go = () => navigate("lookup", { q: input.value.trim() });
	input.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); go(); } });
	const items = parseLinks(q);
	const blocked = items.some((i) => i.kind === "user" || i.kind === "server")
		? new Set(((await api("GET", `${MOD}/blocklist`).catch(() => null))?.blocked || []).map((b) => b.target))
		: new Set();
	fill(main,
		h("h1", {}, "Look up"),
		h("p", { class: "muted" }, "Use your client's Copy link on a picture, message, person or room, paste it here and act on it. Several at once work too, one per line."),
		input,
		h("div", { class: "toolbar" }, h("button", { onclick: go }, "Look up")),
		h("div", { class: "lookup-results" }, items.map((i) => lookupCard(i, blocked))));
	if (!q) input.focus();
};

function lookupCard(item, blocked) {
	const card = h("section", { class: "card" }, h("h2", { class: "mono" }, item.id), h("p", { class: "muted" }, "Loading…"));
	const body = (...children) => fill(card, h("h2", { class: "mono break" }, item.id), ...children);
	const blockedTag = (t) => blocked.has(t) ? h("span", { class: "tag bad" }, "blocked") : null;
	(async () => {
		try {
			if (item.kind === "media") {
				const [server, mediaId] = item.id.replace("mxc://", "").split("/");
				const info = await api("GET", `/_anoncat/admin/v1/media/item/${enc(server)}/${enc(mediaId)}`);
				const thumb = mediaThumb(item.id, info.media_type);
				body(
					h("p", { class: "small muted" }, [
						info.local ? `uploaded here${info.uploader ? ` by ${info.uploader}` : ""}` : `from ${server}${info.server_blocked ? " (blocked)" : ""}`,
						info.stored ? null : info.local ? "not on this server" : "no copy here yet",
						info.media_type, info.bytes ? bytes(info.bytes) : null, info.created_ts ? date(info.created_ts) : null,
					].filter(Boolean).join(" · ")),
					thumb,
					mediaActions(item.id, { info }),
					info.piece ? h("p", {}, h("span", { class: "tag bad" }, "split-file piece"), " Named like a piece of a split-up, encrypted file: someone is using a server as file storage.") : null,
					h("h3", {}, "Posted in"),
					info.rooms?.length
						? h("ul", { class: "room-refs" }, info.rooms.map((r) => h("li", {},
							h("span", {}, r.name || r.room_id), r.name ? h("span", { class: "small muted mono" }, ` ${r.room_id}`) : null, " ",
							h("button", { class: "ghost tiny", onclick: () => lookAt({ kind: "room", room_id: r.room_id }, r.name || r.room_id) }, "Look at room"),
							h("button", { class: "ghost tiny", onclick: () => navigate("lookup", { q: r.room_id }) }, "Room actions"))))
						: h("p", { class: "muted small" }, "No room here that isn't encrypted has posted it. Someone here asked for it directly, or it was posted in an encrypted room, where the server can't read which file a message carries."),
					info.stored ? null : h("p", { class: "muted small" }, "Swapping it now means nobody here ever loads it, even before anyone has opened it."));
			} else if (item.kind === "user") {
				const local = serverOf(item.id) === ourServer();
				const details = local ? await api("GET", `/_synapse/admin/v2/users/${enc(item.id)}`).catch(() => null) : null;
				body(
					h("p", {}, blockedTag(item.id), blocked.has(serverOf(item.id)) ? h("span", { class: "tag bad" }, "server blocked") : null,
						local ? (details ? `${details.displayname || ""} · account here` : "No such account here.") : `On ${serverOf(item.id)}`),
					h("div", { class: "toolbar" },
						h("button", { class: "ghost small", onclick: () => copyText(item.id) }, "Copy"),
						blocked.has(item.id) ? h("button", { class: "ghost small", onclick: () => navigate("blocklist") }, "In the blocklist") :
							h("button", { class: "danger small", onclick: async () => { if (await blockTarget(item.id)) render(); } }, "Block"),
						!local && !blocked.has(serverOf(item.id)) ? h("button", { class: "ghost small", onclick: async () => { if (await blockTarget(serverOf(item.id))) render(); } }, `Block ${serverOf(item.id)}`) : null,
						local && details ? h("button", { class: "ghost small", onclick: () => navigate("media", { user: item.id }) }, "Their uploads") : null,
						local && details ? h("button", { class: "ghost small", onclick: () => navigate("users", { q: item.id.slice(1).split(":")[0] }) }, "In Users") : null));
			} else if (item.kind === "room" || item.kind === "alias") {
				const roomId = item.kind === "alias"
					? (await api("GET", `/_matrix/client/v3/directory/room/${enc(item.id)}`)).room_id
					: item.id;
				const details = await api("GET", `/_synapse/admin/v1/rooms/${enc(roomId)}`).catch(() => null);
				body(
					details ? h("div", { class: "evidence" }, roomFacts(details)) : h("p", { class: "muted" }, "This server isn't in that room, so it knows nothing about it."),
					item.kind === "alias" ? h("p", { class: "small muted mono" }, roomId) : null,
					h("div", { class: "toolbar" },
						details ? h("button", { class: "small", onclick: () => lookAt({ kind: "room", room_id: roomId }, details.name || roomId) }, "Look at room") : null,
						details && item.event ? h("button", { class: "small", onclick: () => lookAt({ kind: "message", room_id: roomId, event_id: item.event }, "Linked message") }, "Look at message") : null,
						details ? h("button", { class: "ghost small", onclick: () => navigate("media", { room: roomId }) }, "Media in it") : null,
						details ? listButton(roomId, details.public, details.name, () => render()) : null,
						h("button", { class: "ghost small", onclick: () => copyText(roomId) }, "Copy room ID"),
						h("button", { class: "danger small", onclick: () => blockRoom(roomId, details?.name) }, "Block room")));
			} else if (item.kind === "server") {
				const cached = await api("GET", `/_anoncat/admin/v1/media/cached?server=${enc(item.id)}`).catch(() => null);
				const mine = cached?.servers.find((s) => s.server === item.id);
				body(
					h("p", {}, blockedTag(item.id), mine ? `${mine.items} files cached here, ${bytes(mine.bytes)}` : "Nothing cached from it."),
					h("div", { class: "toolbar" },
						blocked.has(item.id) ? h("button", { class: "ghost small", onclick: () => navigate("blocklist") }, "In the blocklist") :
							h("button", { class: "danger small", onclick: async () => { if (await blockTarget(item.id)) render(); } }, "Block server"),
						mine ? h("button", { class: "ghost small", onclick: () => navigate("media", { view: "remote", server: item.id }) }, "Its media") : null));
			} else if (item.kind === "event") {
				body(h("p", { class: "muted" }, "A message ID alone doesn't say which room it's in. Copy the message's link in your client instead; that has both."));
			} else {
				body(h("p", { class: "muted" }, "Not something this recognises. Try an mxc:// link, a matrix.to link, @user:server, !room, #alias:server or a server name."));
			}
		} catch (e) {
			body(h("p", { class: "error" }, `Couldn't look it up: ${e.message}`));
		}
	})();
	return card;
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
	fill(main, h("p", { class: "muted" }, "Loading…"));
	try {
		await pages[page](main, new URLSearchParams(query || ""));
		if (page === "reports") refreshBadges();
	} catch (e) {
		fill(main, h("h1", {}, "Couldn't load this page"), h("p", { class: "error" }, e.message));
	}
}

function showApp() {
	$("#login").hidden = true;
	$("#app").hidden = false;
	$("#whoami").textContent = sessionStorage.getItem(USER_KEY) || "";
	render();
	refreshBadges();
}

// The menu's counts: open reports, and a mark on the dashboard when the
// server can't be reached from outside. Every minute, and after each page.
async function refreshBadges() {
	clearTimeout(refreshBadges.timer);
	refreshBadges.timer = setTimeout(refreshBadges, 60_000);
	if ($("#app").hidden) return;
	const s = await api("GET", "/_anoncat/admin/v1/status").catch(() => null);
	if (!s) return;
	const reports = $("#badge-reports");
	reports.hidden = !s.open_reports;
	reports.textContent = s.open_reports > 99 ? "99+" : String(s.open_reports || "");
	$("#badge-dashboard").hidden = s.reachable?.ok !== false;
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

// The sidebar's paste box: whatever is pasted opens on the Look up page.
$("#lookup-form").addEventListener("submit", (e) => {
	e.preventDefault();
	const input = $("#lookup-input");
	if (!input.value.trim()) return navigate("lookup");
	navigate("lookup", { q: input.value.trim() });
	input.value = "";
});
$("#lookup-input").addEventListener("paste", () => setTimeout(() => $("#lookup-form").requestSubmit()));

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
