//! The AI moderation's panel endpoints: settings, the Reports queue, a
//! preview of reported media (hidden or not) and the assistant chat.

use std::collections::BTreeMap;

use axum::{
	Json,
	extract::{Path, State},
	http::{HeaderMap, HeaderValue, Uri, header},
	response::{IntoResponse, Response},
};
use ruma::Mxc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuwunel_core::{Err, Result};
use tuwunel_service::{
	moderation::{Blocked, Model, Report, Said, Settings, Turn},
};

/// Most reports one request returns.
const LIMIT: usize = 500;

/// Media types the preview hands over: raster images only, which a browser
/// can't run as a document on the panel's origin.
const PREVIEW_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];
/// Videos and sound browsers play, which the panel can play in full.
const PLAYABLE_TYPES: [&str; 9] = [
	"video/mp4",
	"video/webm",
	"video/ogg",
	"audio/mpeg",
	"audio/ogg",
	"audio/mp4",
	"audio/webm",
	"audio/wav",
	"audio/flac",
];
/// Largest video or sound the panel plays; it's held in memory while sent.
const PLAY_MAX_BYTES: usize = 1 << 30;

#[derive(Serialize)]
pub(crate) struct SettingsResponse {
	settings: Settings,
	/// Whether a Venice key is saved (never the key itself).
	key_saved: bool,
	key_file: Option<String>,
	/// The listed rooms by ID: their name, and whether this server has them.
	rooms: BTreeMap<String, RoomInfo>,
}

#[derive(Serialize)]
pub(crate) struct RoomInfo {
	name: Option<String>,
	/// False for a room deleted here, or one nobody here has joined yet, so
	/// there's nothing to check.
	here: bool,
}

pub(crate) async fn get_settings_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<SettingsResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(settings_response(&services, services.moderation.settings()).await))
}

async fn settings_response(services: &crate::State, settings: Settings) -> SettingsResponse {
	let mut rooms = BTreeMap::new();
	for room_id in settings.extra_rooms.iter().chain(&settings.trusted_rooms) {
		rooms.insert(room_id.to_string(), RoomInfo {
			name: services.state_accessor.get_name(room_id).await.ok(),
			here: services
				.state
				.get_room_shortstatehash(room_id)
				.await
				.is_ok(),
		});
	}

	SettingsResponse {
		rooms,
		settings,
		key_saved: services.moderation.key().is_some(),
		key_file: services
			.config
			.ai_key_file
			.as_ref()
			.map(|p| p.display().to_string()),
	}
}

/// The settings as the page sends them, all or some; `extra_rooms` may hold
/// aliases.
pub(crate) async fn put_settings_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(mut body): Json<Value>,
) -> Result<Json<SettingsResponse>> {
	super::require_admin_token(&services, &headers).await?;

	// What isn't sent stays as it is.
	let current = services.moderation.settings();
	let extra: Vec<String> = body
		.as_object_mut()
		.and_then(|body| body.remove("extra_rooms"))
		.and_then(|v| serde_json::from_value(v).ok())
		.unwrap_or_else(|| current.extra_rooms.iter().map(ToString::to_string).collect());

	let mut merged = serde_json::to_value(&current)?;
	if let (Some(merged), Some(sent)) = (merged.as_object_mut(), body.as_object()) {
		merged.extend(sent.clone());
	}
	let Ok(settings) = serde_json::from_value::<Settings>(merged) else {
		return Err!(Request(BadJson("Settings didn't parse.")));
	};

	let settings = services
		.moderation
		.set_settings(settings, &extra)
		.await?;

	Ok(Json(settings_response(&services, settings).await))
}

#[derive(Serialize)]
pub(crate) struct MessageResponse {
	message: String,
}

pub(crate) async fn test_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<MessageResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let message = services.moderation.test().await?;
	Ok(Json(MessageResponse { message }))
}

#[derive(Serialize)]
pub(crate) struct ModelsResponse {
	models: Vec<Model>,
}

pub(crate) async fn models_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<ModelsResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(ModelsResponse { models: services.moderation.models().await? }))
}

#[derive(Serialize)]
pub(crate) struct ReportsResponse {
	reports: Vec<Report>,
}

pub(crate) async fn reports_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
) -> Result<Json<ReportsResponse>> {
	super::require_admin_token(&services, &headers).await?;
	// `?all=true` lists closed reports too.
	let all = uri
		.query()
		.is_some_and(|q| q.split('&').any(|p| p == "all=true"));
	let reports = services
		.moderation
		.reports(all, LIMIT)
		.await;
	Ok(Json(ReportsResponse { reports }))
}

#[derive(Deserialize)]
pub(crate) struct ActRequest {
	action: String,
}

pub(crate) async fn act_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(id): Path<u64>,
	Json(body): Json<ActRequest>,
) -> Result<Json<Report>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(services.moderation.act(id, &body.action).await?))
}

/// Media for the admin to judge, served even while hidden: a small still
/// with `?thumb=1` (a frame for a video, when frames are set up), else the
/// file itself, pictures and playable videos only.
pub(crate) async fn media_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
	uri: Uri,
) -> Result<Response> {
	super::require_admin_token(&services, &headers).await?;

	let mxc = format!("mxc://{server}/{media_id}");
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	let thumb = uri
		.query()
		.is_some_and(|q| q.split('&').any(|p| p == "thumb=1"));
	let convert = uri
		.query()
		.is_some_and(|q| q.split('&').any(|p| p == "convert=1"));

	// A picture's preview is made here from the file itself: a stored
	// thumbnail may be another server's, and could show an innocent crop of
	// something that isn't. Its size and frame count go along, so the panel
	// can say when it's long or animated.
	// A swapped item's file is gone; the admin sees what everyone else now
	// does in its place.
	let original = if services.media.is_swapped(&mxc) {
		let choice = services
			.moderation
			.stand_in_image()
			.unwrap_or_else(|| "orange".into());
		services.media.stand_in_picture(&choice)
	} else {
		services.media.get_stored(&mxc).await?
	};
	let is_picture = original
		.content_type
		.as_deref()
		.is_some_and(|t| PREVIEW_TYPES.contains(&t));
	if thumb && is_picture {
		let preview = services
			.media
			.admin_preview_owned(original.content)
			.await?;
		let mut response = preview
			.views
			.into_iter()
			.next()
			.unwrap_or_default()
			.into_response();
		let headers = response.headers_mut();
		headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
		headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
		headers.insert("x-anoncat-width", HeaderValue::from(preview.width));
		headers.insert("x-anoncat-height", HeaderValue::from(preview.height));
		headers.insert("x-anoncat-frames", HeaderValue::from(preview.frames));
		return Ok(response);
	}

	// A video's preview is frames from across it, taken here from the file:
	// its stored thumbnail may be the sender's or another server's choice.
	let is_video = original
		.content_type
		.as_deref()
		.is_some_and(|t| t.starts_with("video/"));
	if thumb && is_video {
		let sheet = services.media.video_sheet(&original.content).await?;
		let mut response = sheet.into_response();
		let headers = response.headers_mut();
		headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
		headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
		headers.insert("x-anoncat-sheet", HeaderValue::from_static("1"));
		return Ok(response);
	}

	// With `?convert=1`, a copy the browser can surely play, for a video
	// its own codec left frozen on the first frame.
	if convert && is_video {
		let playable = services
			.media
			.video_playable(&original.content)
			.await?;
		let mut response = playable.into_response();
		let headers = response.headers_mut();
		headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/webm"));
		headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
		return Ok(response);
	}

	// Anything else has no preview made from the file, and a stored
	// thumbnail isn't trusted to show what the file is.
	if thumb {
		return Err!(Request(Unrecognized("No preview for this kind of file.")));
	}
	let media = original;

	let Some(kind) = media.content_type.filter(|t| {
		PREVIEW_TYPES.contains(&t.as_str()) || (!thumb && PLAYABLE_TYPES.contains(&t.as_str()))
	}) else {
		return Err!(Request(Unrecognized("Only pictures and videos can be previewed here.")));
	};
	if !thumb && PLAYABLE_TYPES.contains(&kind.as_str()) && media.content.len() > PLAY_MAX_BYTES {
		return Err!(Request(TooLarge("Too big to play in the panel.")));
	}

	let mut response = media.content.into_response();
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&kind)?);
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));

	Ok(response)
}

#[derive(Deserialize)]
pub(crate) struct AssistantRequest {
	messages: Vec<Turn>,
}

#[derive(Serialize)]
pub(crate) struct AssistantResponse {
	/// What it said and the commands it ran on its own, in order.
	turns: Vec<Said>,
}

pub(crate) async fn assistant_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<AssistantRequest>,
) -> Result<Json<AssistantResponse>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	let turns = services
		.moderation
		.assistant(&body.messages, &admin)
		.await?;
	Ok(Json(AssistantResponse { turns }))
}

/// `POST .../moderation/rooms/check`: checks every room someone here is in.
pub(crate) async fn check_rooms_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<MessageResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let count = services.moderation.check_all_rooms().await?;
	Ok(Json(MessageResponse {
		message: format!(
			"Checking {count} rooms. Anything that looks wrong shows up under Reports and as a \
			 notice."
		),
	}))
}

#[derive(Serialize)]
pub(crate) struct BlocklistResponse {
	blocked: Vec<Blocked>,
}

pub(crate) async fn blocklist_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<BlocklistResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(BlocklistResponse { blocked: services.moderation.blocklist() }))
}

#[derive(Deserialize)]
pub(crate) struct BlockRequest {
	/// A user ID or a server name.
	target: String,
	#[serde(default)]
	reason: String,
	/// Lock a local user's account too.
	#[serde(default = "yes")]
	lock: bool,
}

const fn yes() -> bool { true }

/// `POST .../moderation/blocklist`: blocks a user or a whole server.
pub(crate) async fn block_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<BlockRequest>,
) -> Result<Json<Blocked>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	let blocked = services
		.moderation
		.block(&body.target, &body.reason, &admin, body.lock)
		.await?;

	Ok(Json(blocked))
}

/// `DELETE .../moderation/blocklist/{target}`
pub(crate) async fn unblock_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(target): Path<String>,
) -> Result<Json<Blocked>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	Ok(Json(services.moderation.unblock(&target, &admin).await?))
}

#[derive(Deserialize)]
pub(crate) struct BacklogBody {
	#[serde(default)]
	server: Option<String>,
}

/// `POST /_anoncat/admin/v1/moderation/backlog`: has the AI check media
/// already stored from other servers (one server's, or all).
pub(crate) async fn backlog_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<BacklogBody>,
) -> Result<Json<tuwunel_service::moderation::BadScan>> {
	super::require_admin_token(&services, &headers).await?;
	services
		.moderation
		.start_backlog(body.server.filter(|s| !s.is_empty()))?;

	Ok(Json(services.moderation.backlog_status()))
}

/// `GET /_anoncat/admin/v1/moderation/backlog`: how far that check got.
pub(crate) async fn backlog_status_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<tuwunel_service::moderation::BadScan>> {
	super::require_admin_token(&services, &headers).await?;

	Ok(Json(services.moderation.backlog_status()))
}

#[derive(Deserialize)]
pub(crate) struct NoteBody {
	#[serde(default)]
	note: String,
	/// The report the note was written about, so the note can say what it
	/// covers.
	#[serde(default)]
	report: Option<u64>,
	/// "add" (the default), "tidy" or "undo".
	#[serde(default)]
	action: String,
}

#[derive(Serialize)]
pub(crate) struct NotesResponse {
	guidance: String,
}

/// `POST /_anoncat/admin/v1/moderation/notes`: adds a note for the AI and
/// has it merged in, tidies the notes, or undoes the last tidy.
pub(crate) async fn notes_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<NoteBody>,
) -> Result<Json<NotesResponse>> {
	super::require_admin_token(&services, &headers).await?;
	let moderation = &services.moderation;
	let guidance = match body.action.as_str() {
		| "tidy" => moderation.tidy_notes().await?,
		| "undo" => moderation.undo_tidy()?,
		| _ => moderation.add_note(&body.note, body.report).await?,
	};

	Ok(Json(NotesResponse { guidance }))
}

/// How long a play key opens its file.
const PLAY_KEY_LIFE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Play keys kept at once; the oldest goes first.
const PLAYS_KEPT: usize = 6;

/// A file the panel's player may fetch by key. Some browsers (WebKitGTK's,
/// through GStreamer) don't play a video from a blob: URL, only from a real
/// one, and a `<video>` can't send the admin's token: the key, made by an
/// admin's request and good for one file for a while, stands in for it.
struct Play {
	mxc: String,
	/// The converted copy, made once; the original is read when fetched.
	converted: Option<axum::body::Bytes>,
	kind: String,
	made: std::time::Instant,
}

static PLAYS: std::sync::LazyLock<std::sync::Mutex<Vec<(String, Play)>>> =
	std::sync::LazyLock::new(Default::default);

#[derive(Default, Deserialize)]
pub(crate) struct PlayRequest {
	#[serde(default)]
	convert: bool,
}

#[derive(Serialize)]
pub(crate) struct PlayResponse {
	url: String,
	converted: bool,
	media_type: String,
	bytes: usize,
}

/// `POST .../moderation/media/{server}/{media_id}/play`: a key the panel's
/// player fetches the file with, or with `convert`, a copy any browser plays.
pub(crate) async fn play_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path((server, media_id)): Path<(String, String)>,
	Json(body): Json<PlayRequest>,
) -> Result<Json<PlayResponse>> {
	super::require_admin_token(&services, &headers).await?;

	let mxc = format!("mxc://{server}/{media_id}");
	let parts: Mxc<'_> = mxc.as_str().try_into()?;
	if services.media.is_swapped(&parts) {
		return Err!(Request(NotFound("Swapped for the stand-in: the file is gone.")));
	}
	let original = services.media.get_stored(&parts).await?;
	let kind = original.content_type.unwrap_or_default();
	let playable = kind.starts_with("video/") || kind.starts_with("audio/");
	if !playable {
		return Err!(Request(Unrecognized("Not a video or sound file.")));
	}
	// An attachment sent in an encrypted room keeps the type its app gave
	// it, but its bytes are scrambled: no player or converter can read it,
	// and the browser would only say "error 4".
	let content = &original.content;
	let video_bytes = tuwunel_service::media::looks_like_video(content)
		|| content.starts_with(b"OggS")
		|| content.starts_with(&[0, 0, 1, 0xBA]);
	if kind.starts_with("video/") && !video_bytes {
		return Err!(Request(Unrecognized(
			"It's labelled a video, but it doesn't start like any video file: most likely an \
			 encrypted attachment from an encrypted room. Only the people in that room can open \
			 it; nobody can play or check it here."
		)));
	}

	let (converted, kind, bytes) = if body.convert {
		if !kind.starts_with("video/") {
			return Err!(Request(Unrecognized("Only videos can be converted.")));
		}
		let copy = services
			.media
			.video_playable(&original.content)
			.await?;
		let len = copy.len();
		(Some(axum::body::Bytes::from(copy)), "video/webm".to_owned(), len)
	} else {
		if original.content.len() > PLAY_MAX_BYTES {
			return Err!(Request(TooLarge("Too big to play in the panel.")));
		}
		(None, kind, original.content.len())
	};

	let key = tuwunel_core::utils::rand::string(32);
	{
		let mut plays = PLAYS.lock().expect("locked");
		plays.retain(|(_, p)| p.made.elapsed() < PLAY_KEY_LIFE);
		while plays.len() >= PLAYS_KEPT {
			plays.remove(0);
		}
		plays.push((key.clone(), Play {
			mxc,
			converted,
			kind: kind.clone(),
			made: std::time::Instant::now(),
		}));
	}

	Ok(Json(PlayResponse {
		url: format!("/_anoncat/admin/v1/moderation/play/{key}"),
		converted: body.convert,
		media_type: kind,
		bytes,
	}))
}

/// `GET .../moderation/play/{key}`: the file a play key opens, with byte
/// ranges, which players use to seek and to read a file's index at its end.
pub(crate) async fn play_file_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(key): Path<String>,
) -> Result<Response> {
	let found = {
		let plays = PLAYS.lock().expect("locked");
		plays
			.iter()
			.find(|(k, p)| *k == key && p.made.elapsed() < PLAY_KEY_LIFE)
			.map(|(_, p)| (p.mxc.clone(), p.converted.clone(), p.kind.clone()))
	};
	let Some((mxc, converted, kind)) = found else {
		return Err!(Request(NotFound("This play link has expired; press play again.")));
	};

	let body = match converted {
		| Some(bytes) => bytes,
		| None => {
			let parts: Mxc<'_> = mxc.as_str().try_into()?;
			if services.media.is_swapped(&parts) {
				return Err!(Request(NotFound("Swapped for the stand-in: the file is gone.")));
			}
			axum::body::Bytes::from(services.media.get_stored(&parts).await?.content)
		},
	};

	let total = body.len();
	let range = headers
		.get(header::RANGE)
		.and_then(|v| v.to_str().ok())
		.and_then(|v| byte_range(v, total));

	let mut response = match range {
		| Some((start, end)) => {
			let mut response = body.slice(start..=end).into_response();
			*response.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
			response.headers_mut().insert(
				header::CONTENT_RANGE,
				HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))?,
			);
			response
		},
		| None => body.into_response(),
	};
	let headers = response.headers_mut();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&kind)?);
	headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
	headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));

	Ok(response)
}

/// A single `bytes=` range, inclusive, within `total`: `a-b`, `a-` or `-n`.
fn byte_range(value: &str, total: usize) -> Option<(usize, usize)> {
	let spec = value.trim().strip_prefix("bytes=")?;
	if spec.contains(',') || total == 0 {
		return None;
	}
	let (start, end) = spec.split_once('-')?;
	let last = total - 1;
	let (start, end) = match (start.trim(), end.trim()) {
		| ("", n) => {
			let n: usize = n.parse().ok()?;
			(total.saturating_sub(n), last)
		},
		| (a, "") => (a.parse().ok()?, last),
		| (a, b) => (a.parse().ok()?, b.parse::<usize>().ok()?.min(last)),
	};

	(start <= end && start <= last).then_some((start, end))
}

#[cfg(test)]
mod play_tests {
	use super::byte_range;

	#[test]
	fn ranges() {
		assert_eq!(byte_range("bytes=0-", 10), Some((0, 9)));
		assert_eq!(byte_range("bytes=2-4", 10), Some((2, 4)));
		assert_eq!(byte_range("bytes=5-99", 10), Some((5, 9)));
		assert_eq!(byte_range("bytes=-3", 10), Some((7, 9)));
		assert_eq!(byte_range("bytes=10-", 10), None);
		assert_eq!(byte_range("bytes=0-1,4-5", 10), None);
	}
}
