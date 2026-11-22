//! Partial requests (`Range: bytes=…`) for media downloads, answered with
//! 206 Partial Content. Video players and browsers ask for pieces of a file
//! to seek and to start playing before the whole file has arrived; without
//! this they get the whole file every time, or refuse to seek.
//!
//! Media is already held in memory whole when it's served, so the piece is
//! cut from the finished response. Only one range per request is answered;
//! anything else (several ranges, `If-Range`, a compressed body) gets the
//! whole file, which the HTTP rules allow.

use axum::{
	body::{Body, to_bytes},
	extract::Request,
	middleware::Next,
	response::Response,
};
use http::{
	HeaderValue, Method, StatusCode,
	header::{
		ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
		IF_RANGE, RANGE,
	},
};

/// Where files are downloaded from by apps.
const DOWNLOADS: &[&str] = &[
	"/_matrix/client/v1/media/download/",
	"/_matrix/media/v3/download/",
	"/_matrix/media/r0/download/",
];

pub(crate) async fn handle(mut req: Request, next: Next) -> Response {
	let path = req.uri().path();
	if !matches!(*req.method(), Method::GET | Method::HEAD)
		|| !DOWNLOADS.iter().any(|p| path.starts_with(p))
	{
		return next.run(req).await;
	}

	let range = req
		.headers()
		.get(RANGE)
		.and_then(|v| v.to_str().ok())
		.map(str::to_owned)
		.filter(|_| req.method() == Method::GET && !req.headers().contains_key(IF_RANGE));
	if range.is_some() {
		// Byte positions count in the file as stored, not a compressed copy.
		req.headers_mut().remove(ACCEPT_ENCODING);
	}

	let mut response = next.run(req).await;
	if response.status() != StatusCode::OK || response.headers().contains_key(CONTENT_ENCODING) {
		return response;
	}
	response
		.headers_mut()
		.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
	let Some(range) = range else {
		return response;
	};

	let (mut parts, body) = response.into_parts();
	let Ok(bytes) = to_bytes(body, usize::MAX).await else {
		return Response::from_parts(parts, Body::empty());
	};
	let len = bytes.len() as u64;

	match parse(&range, len) {
		| Some(Ok((start, end))) => {
			parts.status = StatusCode::PARTIAL_CONTENT;
			parts
				.headers
				.insert(CONTENT_RANGE, header(&format!("bytes {start}-{end}/{len}")));
			parts
				.headers
				.insert(CONTENT_LENGTH, header(&(end - start + 1).to_string()));
			#[expect(
				clippy::cast_possible_truncation,
				reason = "both are within the body"
			)]
			let piece = bytes.slice(start as usize..=end as usize);
			Response::from_parts(parts, Body::from(piece))
		},
		| Some(Err(())) => {
			parts.status = StatusCode::RANGE_NOT_SATISFIABLE;
			parts
				.headers
				.insert(CONTENT_RANGE, header(&format!("bytes */{len}")));
			parts
				.headers
				.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
			parts
				.headers
				.remove(http::header::CONTENT_DISPOSITION);
			Response::from_parts(parts, Body::empty())
		},
		| None => Response::from_parts(parts, Body::from(bytes)),
	}
}

fn header(value: &str) -> HeaderValue {
	HeaderValue::from_str(value).expect("digits, spaces and dashes are a valid header")
}

/// The first and last byte asked for. `None`: not a single byte range we
/// answer, so the whole file goes out. `Err`: a range wholly past the end.
fn parse(range: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
	let spec = range.trim().strip_prefix("bytes=")?.trim();
	if spec.contains(',') {
		return None;
	}
	let (first, last) = spec.split_once('-')?;
	let (first, last) = (first.trim(), last.trim());

	if first.is_empty() {
		// The last N bytes.
		let n: u64 = last.parse().ok()?;
		return Some(if n == 0 || len == 0 {
			Err(())
		} else {
			Ok((len.saturating_sub(n), len - 1))
		});
	}

	let start: u64 = first.parse().ok()?;
	let end = match last {
		| "" => u64::MAX,
		| last => last.parse().ok()?,
	};
	if end < start {
		return None;
	}
	if start >= len {
		return Some(Err(()));
	}
	Some(Ok((start, end.min(len - 1))))
}

#[cfg(test)]
mod tests {
	use super::parse;

	#[test]
	fn ranges() {
		assert_eq!(parse("bytes=0-9", 100), Some(Ok((0, 9))));
		assert_eq!(parse("bytes=90-", 100), Some(Ok((90, 99))));
		assert_eq!(parse("bytes=50-500", 100), Some(Ok((50, 99))));
		assert_eq!(parse("bytes=-10", 100), Some(Ok((90, 99))));
		assert_eq!(parse("bytes=-500", 100), Some(Ok((0, 99))));
		assert_eq!(parse("bytes=100-", 100), Some(Err(())));
		assert_eq!(parse("bytes=-0", 100), Some(Err(())));
		assert_eq!(parse("bytes=0-1,5-6", 100), None);
		assert_eq!(parse("bytes=9-0", 100), None);
		assert_eq!(parse("items=0-9", 100), None);
		assert_eq!(parse("bytes=x-9", 100), None);
	}
}
