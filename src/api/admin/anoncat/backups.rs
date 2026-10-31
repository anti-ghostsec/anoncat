//! The panel's Backups page: the stored online backups, a backup on demand
//! and a check of one. Backups are made by `services.backup`, the same code
//! the backup timer reaches through SIGUSR2.

use axum::{
	Json,
	extract::{Path, State},
	http::HeaderMap,
};
use serde::Serialize;
use tuwunel_core::{Err, Result};
use tuwunel_service::backup::Status;

pub(crate) async fn get_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Status>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(services.backup.status().await))
}

/// Starts a backup in the background; the page polls for it to finish.
pub(crate) async fn run_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<Status>> {
	super::require_admin_token(&services, &headers).await?;
	if services.config.database_backup_path.is_none() {
		return Err!(Request(Unknown("Set database_backup_path in the config to make backups.")));
	}

	services.backup.start("panel")?;
	Ok(Json(services.backup.status().await))
}

#[derive(Serialize)]
pub(crate) struct VerifyResponse {
	id: u32,
	ok: bool,
	message: String,
}

/// Checks a backup's files (0 = the newest). Reads the whole backup.
pub(crate) async fn verify_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(id): Path<u32>,
) -> Result<Json<VerifyResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(match services.backup.verify(id).await {
		| Ok(id) => VerifyResponse { id, ok: true, message: format!("Backup #{id} is intact.") },
		| Err(e) => VerifyResponse { id, ok: false, message: e.to_string() },
	}))
}
