//! The storage check: accounts using this server as file storage, and what
//! the upload limits look like (see the moderation service's `storage`).

use axum::{Json, extract::State, http::HeaderMap};
use ruma::OwnedUserId;
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;
use tuwunel_service::moderation::{IndexStatus, Uploader};

#[derive(Serialize)]
pub(crate) struct StorageResponse {
	index: IndexStatus,
	upload_guard: bool,
	upload_allowance: u64,
	uploaders: Vec<Uploader>,
}

/// `GET /_anoncat/admin/v1/media/storage`
pub(crate) async fn storage_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<StorageResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let settings = services.moderation.settings();

	Ok(Json(StorageResponse {
		index: services.moderation.index_status().await,
		upload_guard: settings.upload_guard,
		upload_allowance: settings.upload_allowance,
		uploaders: services.moderation.uploaders().await,
	}))
}

/// `POST /_anoncat/admin/v1/media/storage/reindex`: read every stored event
/// again for where media appears.
pub(crate) async fn reindex_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<IndexStatus>> {
	super::require_admin_token(&services, &headers).await?;
	services.moderation.start_index_scan();

	Ok(Json(services.moderation.index_status().await))
}

#[derive(Deserialize)]
pub(crate) struct DeleteRequest {
	user_id: OwnedUserId,
	/// Only uploads named like split-file pieces; otherwise everything.
	only_pieces: bool,
}

#[derive(Serialize)]
pub(crate) struct DeletedResponse {
	items: usize,
	bytes: u64,
}

/// `POST /_anoncat/admin/v1/media/storage/delete`: a local account's uploads.
pub(crate) async fn delete_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<DeleteRequest>,
) -> Result<Json<DeletedResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let (items, bytes) = services
		.moderation
		.delete_uploads(&body.user_id, body.only_pieces)
		.await?;

	Ok(Json(DeletedResponse { items, bytes }))
}
