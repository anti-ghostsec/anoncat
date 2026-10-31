// anoncat unlock page: shown on the panel's address while the vault is
// locked. Sends the passphrase to the local unlock page server, which passes
// it to the root helper, then waits for the real panel to answer here.
"use strict";

const $ = (sel) => document.querySelector(sel);

function say(text, error = true) {
	$("#message").className = error ? "error" : "muted";
	$("#message").textContent = text;
}

async function state() {
	try {
		const res = await fetch("/_anoncat/unlock/status", { cache: "no-store", credentials: "omit" });
		const body = await res.json();
		if (body.state === "unlocked") return waitForPanel();
		$("#state").textContent = body.state ? `Vault: ${body.state}` : (body.message || "");
	} catch {
		$("#state").textContent = "";
	}
}

// The panel replaces this server on the same address; reload once it's there.
async function waitForPanel() {
	say("Unlocked. Starting the server, which can take a minute…", false);
	$("#passphrase").disabled = true;
	$("#unlock").disabled = true;
	for (;;) {
		await new Promise((r) => setTimeout(r, 2000));
		try {
			const res = await fetch("/_anoncat/panel/panel.js", { cache: "no-store", credentials: "omit" });
			if (res.ok) return location.reload();
		} catch { /* not up yet */ }
	}
}

$("#unlock-form").addEventListener("submit", async (e) => {
	e.preventDefault();
	const input = $("#passphrase");
	const bytes = new TextEncoder().encode(input.value);
	input.value = "";
	if (!bytes.length) return;
	$("#unlock").disabled = true;
	say("Unlocking… (deriving the key takes a few seconds)", false);
	try {
		const res = await fetch("/_anoncat/unlock", {
			method: "POST",
			headers: { "Content-Type": "application/octet-stream", "X-Anoncat-Unlock": "1" },
			body: bytes,
			cache: "no-store",
			credentials: "omit",
		});
		bytes.fill(0);
		const body = await res.json().catch(() => ({}));
		if (body.ok) return waitForPanel();
		say(body.message || `Failed (HTTP ${res.status}).`);
	} catch (err) {
		// The page server goes away once an unlock worked, sometimes before
		// its answer arrives: if it's gone, the server is starting.
		try {
			const res = await fetch("/_anoncat/unlock/status", { cache: "no-store", credentials: "omit" });
			const body = await res.json();
			if (body.state === "unlocked") return waitForPanel();
			say(`Failed: ${err.message}`);
		} catch {
			return waitForPanel();
		}
	}
	$("#unlock").disabled = false;
	input.focus();
});

state();
