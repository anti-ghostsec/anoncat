//! anoncat's own web pages (`web_signup`): /register, a sign-up form behind a
//! picture captcha the server draws itself, and /login with the account page
//! behind it. No scripts, nothing loaded from anywhere else, and the pages
//! work in Tor Browser on its safest setting.

mod account;
mod home;
mod register;

use std::sync::LazyLock;

use axum::{
	Router,
	response::{Html, IntoResponse, Redirect, Response},
	routing::{get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use http::{
	HeaderMap, HeaderValue, StatusCode, Uri,
	header::{
		CACHE_CONTROL, CONTENT_SECURITY_POLICY, HOST, ORIGIN, REFERRER_POLICY,
		X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
	},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tuwunel_core::{
	Error,
	utils::{hash::sha256, html::escape as html_escape},
};
use tuwunel_service::{Services, signup::Session};

use crate::State;

/// The stylesheet, inlined in every page so a page is one request: each one
/// goes through the VPN and the relay, so a second costs a full round trip.
static STYLE: &str = include_str!("web.css");

/// Pages are inert: only their own stylesheet (by its hash), the captcha as a
/// data: picture, and forms that post only back here.
static CSP: LazyLock<HeaderValue> = LazyLock::new(|| {
	let hash = STANDARD.encode(sha256::hash(STYLE));
	HeaderValue::from_str(&format!(
		"default-src 'none'; style-src 'sha256-{hash}'; img-src data:; form-action 'self'; \
		 frame-ancestors 'none'; base-uri 'none'"
	))
	.expect("a valid header")
});

/// `__Host-` keeps the cookie to this exact host, over HTTPS, for every path.
const COOKIE: &str = "__Host-anoncat";

pub(crate) fn routes(router: Router<State>) -> Router<State> {
	router
		.route("/", get(home::index_route))
		.route("/rules", get(home::rules_route))
		.route("/register", get(register::get_route).post(register::post_route))
		.route("/login", get(account::login_get_route).post(account::login_post_route))
		.route("/account", get(account::get_route))
		.route("/account/{action}", post(account::post_route))
}

/// The pages exist only while `web_signup` is on.
fn enabled(services: &Services) -> Option<Response> {
	(!services.config.web_signup).then(|| StatusCode::NOT_FOUND.into_response())
}

fn page(services: &Services, status: StatusCode, title: &str, body: &str) -> Response {
	let title = html_escape(title);
	let server = html_escape(services.config.server_name.as_str());
	let html = format!(
		r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="same-origin">
<meta name="robots" content="noindex">
<title>{title} · {server}</title>
<style>{STYLE}</style>
</head>
<body>
<main>
<pre class="cat" aria-hidden="true"> /\_/\
( o.o )
 > ^ <</pre>
<p class="brand">{server}</p>
{body}
</main>
</body>
</html>"#
	);

	let mut response = (status, Html(html)).into_response();
	let headers = response.headers_mut();
	headers.insert(CONTENT_SECURITY_POLICY, CSP.clone());
	headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
	headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
	headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
	headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
	response
}

fn see_other(to: &str) -> Response {
	let mut response = Redirect::to(to).into_response();
	response
		.headers_mut()
		.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
	response
}

/// Refuses form posts sent from another site. The pages send their referrer
/// only to themselves (`same-origin`); under `no-referrer` browsers send
/// `Origin: null` on every form post, which looks like another site.
fn same_origin(headers: &HeaderMap, uri: &Uri) -> bool {
	let site = headers
		.get("sec-fetch-site")
		.and_then(|v| v.to_str().ok());

	match site {
		| Some("same-origin") => return true,
		| Some("cross-site" | "same-site") => return false,
		| _ => {},
	}

	let Some(origin) = headers.get(ORIGIN) else {
		return true;
	};

	let host = headers
		.get(HOST)
		.and_then(|v| v.to_str().ok())
		.or_else(|| uri.authority().map(|a| a.as_str()));

	let (Ok(origin), Some(host)) = (origin.to_str(), host) else {
		return false;
	};

	origin
		.strip_prefix("https://")
		.or_else(|| origin.strip_prefix("http://"))
		.is_some_and(|origin| origin.eq_ignore_ascii_case(host))
}

fn forbidden(services: &Services) -> Response {
	page(
		services,
		StatusCode::FORBIDDEN,
		"Refused",
		"<h1>Refused</h1><p>That form was sent from another site.</p>",
	)
}

/// The signed-in session this browser's cookie names, if any.
fn session(services: &Services, jar: &CookieJar) -> Option<Session> {
	let token = jar.get(COOKIE)?.value();
	services.signup.session(token)
}

fn session_cookie(token: String) -> Cookie<'static> {
	Cookie::build((COOKIE, token))
		.path("/")
		.secure(true)
		.http_only(true)
		.same_site(SameSite::Strict)
		.build()
}

fn removal_cookie() -> Cookie<'static> { Cookie::build(COOKIE).path("/").build() }

fn notice(kind: &str, text: &str) -> String {
	match text {
		| "" => String::new(),
		| text => format!(r#"<p class="{kind}" role="status">{}</p>"#, html_escape(text)),
	}
}

/// An error as a person reads it, without the Matrix error code in front.
fn reason(error: &Error) -> String {
	let message = error.sanitized_message();
	match message.split_once(": ") {
		| Some((code, rest)) if code.starts_with("M_") => rest.to_owned(),
		| _ => message,
	}
}
