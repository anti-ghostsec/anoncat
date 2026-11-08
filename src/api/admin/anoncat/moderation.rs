//! The AI moderation's panel endpoints: settings, the Reports queue, a
//! preview of reported media (hidden or not) and the assistant chat.

use axum::{
	Json,
	extract::{Path, State},
	http::{HeaderMap, HeaderValue, Uri, header},
	response::{IntoResponse, Response},
};
use ruma::{Mxc, media::Method};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuwunel_core::{Err, Result};
use tuwunel_service::{
	media::{Animate, Dim},
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
}

pub(crate) async fn get_settings_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<SettingsResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(settings_response(&services, services.moderation.settings())))
}

fn settings_response(services: &crate::State, settings: Settings) -> SettingsResponse {
	SettingsResponse {
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

	Ok(Json(settings_response(&services, settings)))
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

	let media = if thumb {
		let dim = Dim::new(320, 240, Some(Method::Scale));
		services
			.media
			.get_stored_thumbnail(&mxc, &dim, Animate::Never)
			.await?
	} else {
		services.media.get_stored(&mxc).await?
	};

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
