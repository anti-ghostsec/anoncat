//! The panel's Plugins page: what's in the plugins directory, what each
//! manifest asks for, whether an admin approved it, and what went wrong.
//! Load, unload and reload use the same service calls as `!admin plugins`.

use axum::{
	Json,
	extract::{Path, State},
	http::HeaderMap,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result};
use tuwunel_service::plugins::{Alert, Approval, Capabilities};

#[derive(Serialize)]
pub(crate) struct ListResponse {
	dir: String,
	plugins: Vec<Plugin>,
}

#[derive(Serialize)]
struct Plugin {
	name: String,
	loaded: bool,
	/// Loads when the server starts.
	enabled: bool,
	description: String,
	/// The manifest didn't read; nothing else is known then.
	manifest_error: Option<String>,
	capabilities: Option<Capabilities>,
	approval: Option<Approval>,
	/// Whether the stored approval covers what the manifest asks for now.
	approved: bool,
	alerts: Vec<Alert>,
}

pub(crate) async fn list_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
) -> Result<Json<ListResponse>> {
	super::require_admin_token(&services, &headers).await?;
	Ok(Json(list(&services).await?))
}

async fn list(services: &crate::State) -> Result<ListResponse> {
	let plugins = &services.plugins;
	let loaded = plugins.loaded();
	let mut alerts = plugins.alerts();
	let mut out = Vec::new();

	for (name, manifest) in plugins.available()? {
		let approval = plugins.approval(&name).await;
		let is_loaded = loaded.iter().any(|p| p.name == name);
		let alerts = alerts.remove(&name).unwrap_or_default();
		out.push(match manifest {
			| Ok(m) => {
				let capabilities = m.capabilities();
				let approved = capabilities.is_empty()
					|| approval
						.as_ref()
						.is_some_and(|a| a.capabilities.covers(&capabilities));

				Plugin {
					name,
					loaded: is_loaded,
					enabled: m.enabled,
					description: m.description,
					manifest_error: None,
					capabilities: Some(capabilities),
					approval,
					approved,
					alerts,
				}
			},
			| Err(e) => Plugin {
				name,
				loaded: is_loaded,
				enabled: false,
				description: String::new(),
				manifest_error: Some(e.to_string()),
				capabilities: None,
				approval,
				approved: false,
				alerts,
			},
		});
	}

	// A plugin whose files were removed while it ran, or that failed and has
	// only alerts left, still shows.
	for (name, alerts) in alerts {
		out.push(Plugin {
			loaded: loaded.iter().any(|p| p.name == name),
			name,
			enabled: false,
			description: String::new(),
			manifest_error: Some("No longer in the plugins directory.".to_owned()),
			capabilities: None,
			approval: None,
			approved: false,
			alerts,
		});
	}

	Ok(ListResponse {
		dir: plugins.dir().display().to_string(),
		plugins: out,
	})
}

#[derive(Deserialize)]
pub(crate) struct ActRequest {
	/// "load", "unload", "reload", "approve", "revoke" or "clear_alerts".
	action: String,
}

pub(crate) async fn act_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	Path(name): Path<String>,
	Json(body): Json<ActRequest>,
) -> Result<Json<ListResponse>> {
	let admin = super::require_admin_token(&services, &headers).await?;
	let plugins = &services.plugins;

	match body.action.as_str() {
		| "load" | "reload" => plugins.load(&name).await?,
		| "unload" => plugins.unload(&name)?,
		| "approve" => {
			plugins.approve(&name, admin.as_str())?;
		},
		| "revoke" => plugins.revoke(&name)?,
		| "clear_alerts" => plugins.clear_alerts(&name),
		| _ => return Err!(Request(InvalidParam("Unknown action."))),
	}

	Ok(Json(list(&services).await?))
}
