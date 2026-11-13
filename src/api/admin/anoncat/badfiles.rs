//! The known bad files list: fingerprints (SHA-256 and PDQ) of media that
//! must never be taken in again. Listed, exported and imported as text, and
//! stored media can be scanned against it.

use axum::{
	Json,
	extract::State,
	http::{HeaderMap, HeaderValue, header},
	response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;
use tuwunel_service::moderation::{BadFile, BadScan};

/// Most entries one listing returns; the count covers them all.
const LIMIT: usize = 500;

#[derive(Serialize)]
pub(crate) struct ListResponse {
	count: usize,
	entries: Vec<BadFile>,
	scan: BadScan,
}

/// `GET /_anoncat/admin/v1/badfiles`
pub(crate) async fn list_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<ListResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let all = services.moderation.bad_files().await;

	Ok(Json(ListResponse {
		count: all.len(),
		entries: all.into_iter().take(LIMIT).collect(),
		scan: services.moderation.bad_scan_status(),
	}))
}

/// `GET /_anoncat/admin/v1/badfiles/export`: the whole list as text.
pub(crate) async fn export_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Response> {
	super::require_admin_token(&services, &headers).await?;
	let mut response = services
		.moderation
		.export_bad_files()
		.await
		.into_response();
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

	Ok(response)
}

#[derive(Deserialize)]
pub(crate) struct ImportBody {
	text: String,
	#[serde(default)]
	note: String,
}

#[derive(Serialize)]
pub(crate) struct ImportResponse {
	added: usize,
	skipped: usize,
}

/// `POST /_anoncat/admin/v1/badfiles/import`
pub(crate) async fn import_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<ImportBody>,
) -> Result<Json<ImportResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let (added, skipped) = services
		.moderation
		.import_bad_files(&body.text, &body.note);

	Ok(Json(ImportResponse { added, skipped }))
}

#[derive(Deserialize)]
pub(crate) struct RemoveBody {
	kind: String,
	hash: String,
}

/// `POST /_anoncat/admin/v1/badfiles/remove`
pub(crate) async fn remove_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<RemoveBody>,
) -> Result<Json<serde_json::Value>> {
	super::require_admin_token(&services, &headers).await?;
	services
		.moderation
		.remove_bad_file(&body.kind, &body.hash)?;

	Ok(Json(serde_json::json!({})))
}

/// `POST /_anoncat/admin/v1/badfiles/scan`: checks everything stored.
pub(crate) async fn scan_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<BadScan>> {
	super::require_admin_token(&services, &headers).await?;
	services.moderation.start_bad_scan()?;

	Ok(Json(services.moderation.bad_scan_status()))
}
