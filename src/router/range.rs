//! Media downloads without holding big files in memory. Partial requests
//! (`Range: bytes=…`) are answered with 206 Partial Content, so video
//! players can seek and start playing before the whole file has arrived;
//! a big file downloaded whole is streamed from storage.
//!
//! The reading is done by the download handler (see
//! `tuwunel_service::media::range`); this passes the request in and dresses
//! the answer. Several ranges in one request, or `If-Range`, get the whole
//! file, which HTTP allows.

use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use http::{
	HeaderValue, Method, StatusCode,
	header::{
		ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
		CONTENT_RANGE, IF_RANGE, RANGE,
	},
};
use tuwunel_service::media::range::{Answer, WANTED, Wanted};

/// Where apps download files from.
const DOWNLOADS: &[&str] = &["/_matrix/client/v1/media/download/"];

pub(crate) async fn handle(mut req: Request, next: Next) -> Response {
	if *req.method() != Method::GET
		|| !DOWNLOADS
			.iter()
			.any(|p| req.uri().path().starts_with(p))
	{
		return next.run(req).await;
	}

	let spec = req
		.headers()
		.get(RANGE)
		.and_then(|v| v.to_str().ok())
		.filter(|_| !req.headers().contains_key(IF_RANGE))
		.map(str::to_owned);

	// Byte positions count in the file as stored, not a compressed copy; and
	// a streamed body goes out as stored.
	req.headers_mut().remove(ACCEPT_ENCODING);
	let (response, answer) = WANTED
		.scope(Wanted::new(spec), async move {
			let response = next.run(req).await;
			(response, WANTED.with(Wanted::take))
		})
		.await;

	if response.status() != StatusCode::OK || response.headers().contains_key(CONTENT_ENCODING) {
		return response;
	}
	let (mut parts, body) = response.into_parts();
	parts
		.headers
		.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));

	match answer {
		| Some(Answer::Piece { start, end, total }) => {
			parts.status = StatusCode::PARTIAL_CONTENT;
			parts
				.headers
				.insert(CONTENT_RANGE, header(&format!("bytes {start}-{end}/{total}")));
			parts
				.headers
				.insert(CONTENT_LENGTH, header(&(end - start + 1).to_string()));
			Response::from_parts(parts, body)
		},
		| Some(Answer::Unsatisfiable { total }) => {
			parts.status = StatusCode::RANGE_NOT_SATISFIABLE;
			parts
				.headers
				.insert(CONTENT_RANGE, header(&format!("bytes */{total}")));
			parts
				.headers
				.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
			parts.headers.remove(CONTENT_DISPOSITION);
			Response::from_parts(parts, Body::empty())
		},
		| Some(Answer::Whole { total, body }) => {
			parts
				.headers
				.insert(CONTENT_LENGTH, header(&total.to_string()));
			Response::from_parts(parts, Body::from_stream(body))
		},
		| None => Response::from_parts(parts, body),
	}
}

fn header(value: &str) -> HeaderValue {
	HeaderValue::from_str(value).expect("digits, spaces and dashes are a valid header")
}
