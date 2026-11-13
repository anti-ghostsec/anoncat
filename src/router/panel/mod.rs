//! The anoncat admin panel: a static web app built into the binary, served
//! only on the loopback `admin_panel_address` listener. It talks to the
//! server's own admin APIs on the same origin. Those admin APIs are served
//! only there too: the public listeners get a router without them.

use std::net::SocketAddr;

use axum::{
	Router,
	extract::Request,
	http::{HeaderValue, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Redirect, Response},
	routing::get,
};

const INDEX: &str = include_str!("index.html");
const SCRIPT: &str = include_str!("panel.js");
const STYLE: &str = include_str!("panel.css");

/// Only our own files; images from blob: URLs made from authenticated media
/// downloads; videos from our own play links; nothing may frame the panel.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; media-src 'self' blob:; \
                   connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'";

/// The panel listener: the panel's files plus the whole app, for requests that
/// name the loopback address in `Host`. Anything else is a page elsewhere that
/// rebound its own hostname to this port (DNS rebinding), so it's refused.
pub(crate) fn panel_router(app: Router, addr: SocketAddr) -> Router {
	let port = addr.port();
	let allowed: [String; 3] = [addr.to_string(), format!("localhost:{port}"), format!("[::1]:{port}")];
	let allowed = std::sync::Arc::new(allowed);

	routes().merge(app).layer(middleware::from_fn(move |req: Request, next: Next| {
		let allowed = allowed.clone();
		async move {
			let host = req
				.headers()
				.get(header::HOST)
				.and_then(|host| host.to_str().ok())
				.map(str::to_ascii_lowercase);

			if !host.is_some_and(|host| allowed.contains(&host)) {
				return StatusCode::MISDIRECTED_REQUEST.into_response();
			}

			// A page on another site may name this address in Host too
			// (fetch to http://127.0.0.1:...); the browser says where it
			// came from, and only the panel itself is let in.
			let cross_site = req
				.headers()
				.get("sec-fetch-site")
				.is_some_and(|site| matches!(site.as_bytes(), b"cross-site" | b"same-site"));
			let foreign_origin = req
				.headers()
				.get(header::ORIGIN)
				.and_then(|origin| origin.to_str().ok())
				.is_some_and(|origin| {
					origin
						.to_ascii_lowercase()
						.strip_prefix("http://")
						.is_none_or(|host| !allowed.iter().any(|a| a == host))
				});
			if cross_site || foreign_origin {
				return StatusCode::FORBIDDEN.into_response();
			}

			next.run(req).await
		}
	}))
}

/// The public listeners: the app without anoncat's admin API, and without the
/// Synapse admin API or the client API's admin routes (whois, suspend, lock)
/// when the panel is where admins work.
pub(crate) fn public_router(app: Router, panel: bool) -> Router {
	app.layer(middleware::from_fn(move |req: Request, next: Next| {
		let path = req.uri().path();
		let client_admin = path
			.strip_prefix("/_matrix/client/")
			.and_then(|rest| rest.split_once('/'))
			.is_some_and(|(_version, rest)| rest.starts_with("admin/"));
		let admin = path.starts_with("/_anoncat/")
			|| (panel && (path.starts_with("/_synapse/admin") || client_admin));
		async move {
			if admin {
				return StatusCode::NOT_FOUND.into_response();
			}

			next.run(req).await
		}
	}))
}

fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
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
