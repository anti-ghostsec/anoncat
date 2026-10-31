//! `GET /_anoncat/admin/v1/notices`: what the server has to tell its admins,
//! newest first. anoncat has no admin room; this is where notices go.

use axum::{Json, extract::State, http::HeaderMap};
use serde::Serialize;
use tuwunel_core::Result;
use tuwunel_service::admin::Notice;

/// Most notices one request returns.
const LIMIT: usize = 200;

#[derive(Serialize)]
pub(crate) struct Response {
	notices: Vec<Notice>,
}

pub(crate) async fn anoncat_notices_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Response>> {
	super::require_admin_token(&services, &headers).await?;

	Ok(Json(Response { notices: services.admin.notices(LIMIT).await }))
}
