//! The front page and the rules people accept when they sign up
//! (`registration_terms` points at /rules). Served by anoncat itself, so the
//! server's own address is all anyone needs.

use axum::{extract::State, response::Response};
use http::StatusCode;
use tuwunel_core::utils::html::escape as html_escape;

use super::page;

/// Change "Version" here and `registration_terms.rules.version` in the config
/// together, so people are asked to accept the new rules.
const RULES: &str = r#"<h1>Rules and privacy</h1>
<p class="hint">Version 2</p>
<section>
<h2>Rules</h2>
<ol>
<li>Nothing illegal. Sexual content involving minors means an instant ban.</li>
<li>No spam, scams, malware, or bots unless the admins agree.</li>
<li>No threats, harassment, or posting other people's private details.</li>
</ol>
<p class="hint">Break them and your posts or account may be removed.</p>
</section>
<section>
<h2>Privacy</h2>
<ul>
<li>No email or phone number is asked for. No tracking, no ads, no analytics.</li>
<li>Your IP address is not stored or logged.</li>
<li>Location and camera details are removed from photos you upload (not from videos).</li>
<li>Admins can technically read rooms that are not encrypted. Use encrypted chats for anything private.</li>
<li>Other servers in your rooms get copies of your messages and profile, and we can't delete those.</li>
<li>Messages are deleted after 30 days, or 90 days if they have a picture or file.</li>
<li>Delete your account any time at <a href="/login">/login</a>. Your uploads go with it, and backups keep it for up to 2 more days.</li>
<li>There is no password reset. If you lose your password, the account is gone.</li>
</ul>
</section>
<p class="foot"><a href="/">Back to the front page</a></p>"#;

pub(super) async fn index_route(State(services): State<crate::State>) -> Response {
	if let Some(off) = super::enabled(&services) {
		return off;
	}

	let server = html_escape(services.config.server_name.as_str());
	let body = format!(
		r#"<h1>A private Matrix chat server</h1>
<p>No email, no tracking.</p>
<p><a class="button" href="/register">Create an account</a> <a href="/login">Manage your account</a></p>
<section>
<h2>How to join</h2>
<ol>
<li>Create an account above.</li>
<li>Get a Matrix app, like <a href="https://element.io/download" rel="noopener noreferrer">Element</a>.</li>
<li>Sign in, choose another homeserver, and enter <strong>{server}</strong>.</li>
</ol>
</section>
<p class="foot"><a href="/rules">Rules and privacy</a></p>"#
	);

	page(&services, StatusCode::OK, "Welcome", &body)
}

pub(super) async fn rules_route(State(services): State<crate::State>) -> Response {
	if let Some(off) = super::enabled(&services) {
		return off;
	}

	page(&services, StatusCode::OK, "Rules and privacy", RULES)
}
