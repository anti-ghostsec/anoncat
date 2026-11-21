//! The onion listener: anoncat's apps and web pages reached over Tor, at an
//! onion address that Tor on this machine points at `onion_address`.
//!
//! Tor makes only outgoing connections to reach its relays, so nothing is
//! forwarded to this machine and no port is published: people type the
//! `.onion` name and Tor finds the way. Tor encrypts the whole way, so the
//! listener is plain HTTP on a loopback address.
//!
//! It carries people's apps only. Other servers reach this one by its normal
//! name, so the federation routes aren't served here; and the app's
//! well-known points back at the onion name the request came in on, so an
//! app that started on the onion address stays on it.

use axum::{
	Router,
	extract::Request,
	http::{HeaderValue, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
};

/// The public app, behind the onion's own rules.
pub(crate) fn onion_router(app: Router) -> Router {
	app.layer(middleware::from_fn(async |req: Request, next: Next| {
		// Tor names the onion in `Host`. Anything else is a page or program
		// on this machine using the loopback port directly, or a rebound name.
		let Some(host) = req
			.headers()
			.get(header::HOST)
			.and_then(|host| host.to_str().ok())
			.map(str::to_ascii_lowercase)
			.filter(|host| is_onion(host))
		else {
			return StatusCode::MISDIRECTED_REQUEST.into_response();
		};

		let path = req.uri().path();
		if path.starts_with("/_matrix/federation/")
			|| path.starts_with("/_matrix/key/")
			|| path == "/.well-known/matrix/server"
		{
			return StatusCode::NOT_FOUND.into_response();
		}

		if path == "/.well-known/matrix/client" {
			return well_known_client(&host);
		}

		next.run(req).await
	}))
}

/// A v3 onion name, optionally with a port: 56 base32 letters then `.onion`.
fn is_onion(host: &str) -> bool {
	let name = host
		.rsplit_once(':')
		.filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
		.map_or(host, |(name, _)| name);

	name.strip_suffix(".onion")
		.and_then(|label| label.rsplit('.').next())
		.is_some_and(|label| {
			label.len() == 56
				&& label
					.bytes()
					.all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
		})
}

fn well_known_client(host: &str) -> Response {
	let body = serde_json::json!({ "m.homeserver": { "base_url": format!("http://{host}") } });
	let mut response = axum::Json(body).into_response();
	response
		.headers_mut()
		.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
	response
}

#[cfg(test)]
mod tests {
	use super::is_onion;

	const NAME: &str = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx";

	#[test]
	fn onion_names() {
		assert!(is_onion(&format!("{NAME}.onion")));
		assert!(is_onion(&format!("{NAME}.onion:80")));
		assert!(is_onion(&format!("www.{NAME}.onion")));
		assert!(!is_onion("127.0.0.1:8451"));
		assert!(!is_onion("localhost:8451"));
		assert!(!is_onion("short.onion"));
		assert!(!is_onion(&format!("{NAME}.onion.example.com")));
		assert!(!is_onion(&format!("{}.onion", NAME.replace('a', "1"))));
	}
}
