//! Media cached from other servers: what's kept, per server, and deleting it;
//! and any one item, local or not, looked up by its mxc:// link.

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
	/// Swapped for the stand-in for good; the file is gone.
	swapped: bool,
	/// Seen in a room here. Media nobody here posted got fetched by someone
	/// asking for it directly, or was posted in an encrypted room.
	in_room: bool,
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

	let mut media = Vec::new();
	for item in cached
		.iter()
		.filter(|item| only.as_deref().is_none_or(|s| item.server.as_str() == s))
		.take(LIMIT)
	{
		media.push(Item {
			mxc: item.mxc.to_string(),
			server: item.server.to_string(),
			media_type: item.media_type.clone(),
			bytes: item.bytes,
			fetched_ts: item.fetched_ts,
			hidden: services.media.is_hidden(&item.mxc.parts().expect("listed items are valid")),
			swapped: services.media.is_swapped(&item.mxc.parts().expect("listed items are valid")),
			in_room: services.moderation.in_any_room(item.mxc.as_str()).await,
		});
	}

	// Swapped items' files are gone, so they're not among the cached ones;
	// they're listed anyway, showing the stand-in, so a swap doesn't look
	// like the item vanished.
	for mxc in services.media.swapped().await {
		let Ok(parts) = Mxc::try_from(mxc.as_str()) else {
			continue;
		};
		if services.globals.server_is_ours(parts.server_name)
			|| only.as_deref().is_some_and(|s| parts.server_name.as_str() != s)
			|| media.iter().any(|m| m.mxc == mxc)
		{
			continue;
		}
		media.push(Item {
			server: parts.server_name.to_string(),
			media_type: Some("image/png".into()),
			bytes: 0,
			fetched_ts: 0,
			hidden: true,
			swapped: true,
			in_room: services.moderation.in_any_room(&mxc).await,
			mxc,
		});
	}

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

#[derive(Serialize)]
pub(crate) struct ItemInfo {
	mxc: String,
	/// Uploaded here, rather than a copy of another server's.
	local: bool,
	/// Whether this server holds the file at all.
	stored: bool,
	hidden: bool,
	/// Swapped for the stand-in for good; the file is gone.
	swapped: bool,
	server_blocked: bool,
	/// Its server's media isn't fetched or shown here (Settings).
	media_blocked: bool,
	uploader: Option<String>,
	media_type: Option<String>,
	bytes: Option<u64>,
	upload_name: Option<String>,
	created_ts: Option<u64>,
	/// Rooms it was seen in, as far as this server can tell.
	rooms: Vec<RoomRef>,
	/// Named like a piece of a split-up file.
	piece: bool,
}

#[derive(Serialize)]
pub(crate) struct RoomRef {
	room_id: String,
	name: Option<String>,
}

/// Parses the path's server and ID as an mxc:// link.
fn item_mxc(server: &str, media_id: &str) -> Result<String> {
	let mxc = format!("mxc://{server}/{media_id}");
	if Mxc::try_from(mxc.as_str()).is_err() {
		return Err!(Request(InvalidParam("Not an mxc:// link.")));
	}

	Ok(mxc)
}

/// `GET /_anoncat/admin/v1/media/item/{server}/{media_id}`: what's known about
/// one item, from here or anywhere.
pub(crate) async fn item_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<ItemInfo>> {
	super::require_admin_token(&services, &headers).await?;
	let mxc = item_mxc(&server, &media_id)?;
	let parts: Mxc<'_> = mxc.as_str().try_into()?;
	let entry = services.media.media_entry(&parts).await;
	let local = services.globals.server_is_ours(parts.server_name);
	let mut rooms = Vec::new();
	for room_id in services.moderation.rooms_of(&mxc).await {
		rooms.push(RoomRef {
			name: services.state_accessor.get_name(&room_id).await.ok(),
			room_id: room_id.to_string(),
		});
	}

	Ok(Json(ItemInfo {
		rooms,
		piece: entry
			.as_ref()
			.and_then(|e| e.upload_name.as_deref())
			.is_some_and(tuwunel_service::moderation::piece_name),
		local,
		stored: entry.is_some(),
		hidden: services.media.is_hidden(&parts),
		swapped: services.media.is_swapped(&parts),
		server_blocked: !local && services.moderation.server_blocked(parts.server_name),
		media_blocked: !local
			&& services
				.moderation
				.media_server_blocked(parts.server_name),
		uploader: entry
			.as_ref()
			.and_then(|e| e.user_id.as_ref())
			.filter(|_| local)
			.map(ToString::to_string),
		media_type: entry.as_ref().and_then(|e| e.media_type.clone()),
		bytes: entry.as_ref().and_then(|e| e.media_length),
		upload_name: entry.as_ref().and_then(|e| e.upload_name.clone()),
		created_ts: entry.as_ref().map(|e| e.created_ts),
		mxc,
	}))
}

/// `DELETE /_anoncat/admin/v1/media/item/{server}/{media_id}`: an upload from
/// here is gone for good; another server's is only this server's copy.
pub(crate) async fn delete_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<DeletedResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let mxc = item_mxc(&server, &media_id)?;
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	if services.media.media_entry(&mxc).await.is_none() {
		return Err!(Request(NotFound("This server has no copy of it.")));
	}
	if services.globals.server_is_ours(mxc.server_name) {
		services.media.delete(&mxc).await?;
	} else {
		services.media.delete_cached(&mxc).await?;
	}

	Ok(Json(DeletedResponse { items: 1, bytes: 0 }))
}

#[derive(Serialize)]
pub(crate) struct HiddenResponse {
	hidden: bool,
}

/// `POST /_anoncat/admin/v1/media/item/{server}/{media_id}/hide`: swapped
/// for the stand-in for good. The file is deleted, and everyone, here and on
/// other servers, gets the stand-in picture instead. Works before this server
/// has a copy, so nobody here ever fetches it.
pub(crate) async fn hide_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<HiddenResponse>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	let mxc = item_mxc(&server, &media_id)?;
	services
		.media
		.swap(&mxc, &format!("by {admin}"))
		.await?;
	services.moderation.resolve_media(&mxc).await;

	Ok(Json(HiddenResponse { hidden: true }))
}

/// `DELETE /_anoncat/admin/v1/media/item/{server}/{media_id}/hide`
pub(crate) async fn unhide_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
) -> Result<Json<HiddenResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let mxc = item_mxc(&server, &media_id)?;
	if services.media.is_swapped(&mxc.as_str().try_into()?) {
		return Err!(Request(Forbidden("It was swapped for good; the file is deleted.")));
	}
	services.media.unhide(&mxc)?;

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
