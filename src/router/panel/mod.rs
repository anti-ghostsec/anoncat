//! The anoncat admin panel: a static web app built into the binary, served
//! only on the loopback `admin_panel_address` listener. It talks to the
//! server's own admin APIs on the same origin.

use axum::{
	Router,
	http::{HeaderValue, header},
	response::{IntoResponse, Redirect, Response},
	routing::get,
};

const INDEX: &str = include_str!("index.html");
const SCRIPT: &str = include_str!("panel.js");
const STYLE: &str = include_str!("panel.css");

/// Only our own files; images from blob: URLs made from authenticated media
/// downloads; nothing may frame the panel.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; \
                   connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'";

pub(crate) fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
	Router::new()
		.route("/_anoncat/panel", get(async || { Redirect::permanent("/_anoncat/panel/") }))
		.route("/_anoncat/panel/", get(async || { asset(INDEX, "text/html; charset=utf-8") }))
		.route("/_anoncat/panel/panel.js", get(async || { asset(SCRIPT, "text/javascript; charset=utf-8") }))
		.route("/_anoncat/panel/panel.css", get(async || { asset(STYLE, "text/css; charset=utf-8") }))
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
	let mut response = body.into_response();
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
	headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
	headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
	response
}
