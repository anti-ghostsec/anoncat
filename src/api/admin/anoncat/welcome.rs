//! `/_anoncat/admin/v1/welcome`: the welcome message new accounts get by
//! themselves, set from the panel's Announce page.

use axum::{Json, extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;

#[derive(Deserialize, Serialize)]
pub(crate) struct Welcome {
	/// Markdown; none or blank means no welcome message.
	#[serde(default)]
	text: Option<String>,
}

pub(crate) async fn get_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Welcome>> {
	super::require_admin_token(&services, &headers).await?;

	Ok(Json(Welcome { text: services.notices.welcome().await }))
}

pub(crate) async fn put_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Json(body): Json<Welcome>,
) -> Result<Json<Welcome>> {
	super::require_admin_token(&services, &headers).await?;

	services
		.notices
		.set_welcome(body.text.as_deref())
		.await?;

	Ok(Json(Welcome { text: services.notices.welcome().await }))
}
