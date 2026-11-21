//! The front page and the rules people accept when they sign up
//! (`registration_terms` points at /rules). Served by anoncat itself, so the
//! server's own address is all anyone needs.

use axum::{extract::State, response::Response};
use http::StatusCode;
use tuwunel_core::utils::html::escape as html_escape;

use super::page;

/// Text the admin wrote, safe in a page: escaped, with web addresses and
/// this site's own paths (`/login`) made into links.
fn words(text: &str) -> String {
	text.split(' ')
		.map(|word| {
			let bare = word.trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
			let rest = &word[bare.len()..];
			let link = bare.starts_with("https://")
				|| (bare.starts_with('/') && bare[1..].starts_with(|c: char| c.is_ascii_lowercase()));
			if link && bare.len() > 1 {
				let bare = html_escape(bare);
				format!(r#"<a href="{bare}" rel="noopener noreferrer">{bare}</a>{}"#, html_escape(rest))
			} else {
				html_escape(word).to_string()
			}
		})
		.collect::<Vec<_>>()
		.join(" ")
}

/// A paragraph a line, in the front page's notice box; nothing when empty.
fn notice(text: &str) -> String {
	let paragraphs: String = text
		.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.map(|line| format!("<p>{}</p>", words(line)))
		.collect();

	if paragraphs.is_empty() {
		return String::new();
	}

	format!(r#"<aside class="banner" role="note">{paragraphs}</aside>
"#)
}

/// An item a line.
fn items(text: &str) -> String {
	text.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.map(|line| format!("<li>{}</li>", words(line)))
		.collect()
}

pub(super) async fn index_route(State(services): State<crate::State>) -> Response {
	if let Some(off) = super::enabled(&services) {
		return off;
	}

	let server = html_escape(services.config.server_name.as_str());
	let settings = services.moderation.settings();
	let title = words(&settings.page_title);
	let intro = words(&settings.page_intro);
	let notice = notice(&settings.page_notice);
	let body = format!(
		r#"{notice}<h1 class="hero">{title}</h1>
<p class="lead">{intro}</p>
<div class="actions"><a class="button" href="/register">Create an account</a> <a class="button quiet" href="/login">Manage your account</a></div>
<section>
<h2>How to join</h2>
<ol class="steps">
<li>Create an account above.</li>
<li>Get a Matrix app: <a href="https://element.io/download" rel="noopener noreferrer">Element</a>, or <a href="https://github.com/anti-ghostsec/whisker" rel="noopener noreferrer">Whisker</a>, our own. Whisker is built for privacy first, but it's early in development, so expect rough edges.</li>
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

	let settings = services.moderation.settings();
	let body = format!(
		r#"<h1>Rules and privacy</h1>
<section>
<h2>Rules</h2>
<ol class="rules">{rules}</ol>
<p class="hint">Break them and your posts or account may be removed.</p>
</section>
<section>
<h2>Privacy</h2>
<ul>{privacy}</ul>
</section>
<p class="foot"><a href="/">Back to the front page</a></p>"#,
		rules = items(&settings.page_rules),
		privacy = items(&settings.page_privacy),
	);

	page(&services, StatusCode::OK, "Rules and privacy", &body)
}

#[cfg(test)]
mod tests {
	use super::{items, notice, words};

	#[test]
	fn admin_words_are_escaped_and_linked() {
		assert_eq!(words("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
		assert_eq!(
			words("Delete it at /login."),
			r#"Delete it at <a href="/login" rel="noopener noreferrer">/login</a>."#
		);
		assert_eq!(words("a/b and / alone"), "a/b and / alone");
		assert_eq!(items("one\n\n two "), "<li>one</li><li>two</li>");
		assert_eq!(notice(" \n "), "");
		assert!(notice("<i>a</i>\nb").starts_with(r#"<aside class="banner" role="note"><p>&lt;i&gt;a"#));
		assert!(!words(r#"https://x.org/"onmouseover="a"#).contains(r#"""onmouseover"#));
	}
}
