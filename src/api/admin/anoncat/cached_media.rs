//! Media cached from other servers: what's kept, per server, and deleting it.

use axum::{
	Json,
	body::Bytes,
	extract::{Path, State},
	http::{HeaderMap, Uri, header},
	response::IntoResponse,
};
use ruma::Mxc;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result};

/// Most items one listing returns; the per-server totals cover everything.
const LIMIT: usize = 300;

#[derive(Serialize)]
pub(crate) struct Item {
	mxc: String,
	server: String,
	media_type: Option<String>,
	bytes: u64,
	fetched_ts: u64,
	/// Hidden: people here see the cat, or nothing, in its place.
	hidden: bool,
}

#[derive(Default, Serialize)]
pub(crate) struct ServerTotal {
	server: String,
	items: usize,
	bytes: u64,
	blocked: bool,
}

#[derive(Serialize)]
pub(crate) struct ListResponse {
	/// Whether hidden and blocked media is shown to people here as a cat.
	cat_stand_in: bool,
	items: usize,
	bytes: u64,
	servers: Vec<ServerTotal>,
	/// Biggest first, at most [`LIMIT`]; only `?server=`'s when given.
	media: Vec<Item>,
}

/// `GET /_anoncat/admin/v1/media/cached[?server=name]`
pub(crate) async fn list_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
) -> Result<Json<ListResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let only = uri
		.query()
		.and_then(|q| q.split('&').find_map(|p| p.strip_prefix("server=")))
		.map(|s| urlencoding_decode(s));

	let cached = services.media.cached_remote().await;

	let mut servers: Vec<ServerTotal> = Vec::new();
	for item in &cached {
		match servers.iter_mut().find(|s| s.server == item.server.as_str()) {
			| Some(total) => {
				total.items = total.items.saturating_add(1);
				total.bytes = total.bytes.saturating_add(item.bytes);
			},
			| None => servers.push(ServerTotal {
				server: item.server.to_string(),
				items: 1,
				bytes: item.bytes,
				blocked: services.moderation.server_blocked(&item.server),
			}),
		}
	}
	servers.sort_by(|a, b| b.bytes.cmp(&a.bytes));

	let media = cached
		.iter()
		.filter(|item| only.as_deref().is_none_or(|s| item.server.as_str() == s))
		.take(LIMIT)
		.map(|item| Item {
			mxc: item.mxc.to_string(),
			server: item.server.to_string(),
			media_type: item.media_type.clone(),
			bytes: item.bytes,
			fetched_ts: item.fetched_ts,
			hidden: services.media.is_hidden(&item.mxc.parts().expect("listed items are valid")),
		})
		.collect();

	Ok(Json(ListResponse {
		cat_stand_in: services.moderation.cat_stand_in(),
		items: cached.len(),
		bytes: cached.iter().map(|i| i.bytes).sum(),
		servers,
		media,
	}))
}

#[derive(Serialize)]
pub(crate) struct DeletedResponse {
	items: usize,
	bytes: u64,
}

/// `DELETE /_anoncat/admin/v1/media/cached/{server}/{media_id}`
pub(crate) async fn delete_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<DeletedResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let mxc = format!("mxc://{server}/{media_id}");
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	services.media.delete_cached(&mxc).await?;
	Ok(Json(DeletedResponse { items: 1, bytes: 0 }))
}

#[derive(Serialize)]
pub(crate) struct HiddenResponse {
	hidden: bool,
}

/// `POST /_anoncat/admin/v1/media/cached/{server}/{media_id}/hide`: kept
/// stored for the panel, but served to nobody; people here see the cat.
pub(crate) async fn hide_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<HiddenResponse>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	services
		.media
		.hide(&format!("mxc://{server}/{media_id}"), &format!("hidden by {admin}"))?;

	Ok(Json(HiddenResponse { hidden: true }))
}

/// `DELETE /_anoncat/admin/v1/media/cached/{server}/{media_id}/hide`
pub(crate) async fn unhide_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<HiddenResponse>> {
	super::require_admin_token(&services, &headers).await?;
	services
		.media
		.unhide(&format!("mxc://{server}/{media_id}"))?;

	Ok(Json(HiddenResponse { hidden: false }))
}

#[derive(Deserialize)]
pub(crate) struct PurgeRequest {
	server: String,
}

/// `POST /_anoncat/admin/v1/media/cached/purge`: everything from one server.
pub(crate) async fn purge_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<PurgeRequest>,
) -> Result<Json<DeletedResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let (items, bytes) = services
		.media
		.delete_cached_from(body.server.trim())
		.await?;

	Ok(Json(DeletedResponse { items, bytes }))
}

/// `GET /_anoncat/admin/v1/media/stand-in/{name}`: one of the pictures to
/// choose from, for the panel to show.
pub(crate) async fn stand_in_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(name): Path<String>,
) -> Result<impl IntoResponse> {
	super::require_admin_token(&services, &headers).await?;
	if name == "custom" && services.moderation.custom_stand_in().is_none() {
		return Err!(Request(NotFound("No picture uploaded.")));
	}

	let picture = services.media.stand_in_picture(&name);
	let content_type = picture.content_type.unwrap_or_default();
	Ok((
		[(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store".into())],
		picture.content,
	))
}

#[derive(Serialize)]
pub(crate) struct UploadedResponse {
	content_type: &'static str,
	bytes: usize,
}

/// `PUT /_anoncat/admin/v1/media/stand-in`: the admin's own picture, sent as
/// the raw file. Its metadata is removed before it's kept.
pub(crate) async fn put_stand_in_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Json<UploadedResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let kept = services.moderation.set_custom_stand_in(&body)?;

	Ok(Json(UploadedResponse { content_type: kept.content_type, bytes: kept.content.len() }))
}

/// `DELETE /_anoncat/admin/v1/media/stand-in`
pub(crate) async fn delete_stand_in_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<HiddenResponse>> {
	super::require_admin_token(&services, &headers).await?;
	services.moderation.clear_custom_stand_in();

	Ok(Json(HiddenResponse { hidden: false }))
}

/// Server names are plain ASCII; only `%3A` (a port's colon) needs undoing.
fn urlencoding_decode(s: &str) -> String { s.replace("%3A", ":").replace("%3a", ":") }
