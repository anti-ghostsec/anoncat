use std::fmt::Write;

use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn plugins_list(&self) -> Result {
	let plugins = &self.services.plugins;
	let loaded = plugins.loaded();
	let mut out = format!("Plugins directory: `{}`\n\n", plugins.dir().display());

	for (name, manifest) in plugins.available()? {
		let state = if loaded.iter().any(|p| p.name == name) { "loaded" } else { "not loaded" };
		match manifest {
			| Ok(m) => {
				let mut caps = Vec::new();
				if m.check_events {
					caps.push("check_events".to_owned());
				}
				if m.send_messages {
					caps.push("send_messages".to_owned());
				}
				if !m.events.is_empty() {
					caps.push(format!("events: {}", m.events.join(", ")));
				}

				writeln!(
					out,
					"- **{name}** ({state}{}) {}{}",
					if m.enabled { ", starts with the server" } else { "" },
					m.description,
					if caps.is_empty() { String::new() } else { format!(" — {}", caps.join("; ")) },
				)?;
			},
			| Err(e) => writeln!(out, "- **{name}** ({state}): {e}")?,
		}
	}

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn plugins_load(&self, name: String) -> Result {
	self.services.plugins.load(&name).await?;
	self.write_str(&format!("Loaded {name}.")).await
}

#[admin_command]
pub(super) async fn plugins_unload(&self, name: String) -> Result {
	self.services.plugins.unload(&name)?;
	self.write_str(&format!("Unloaded {name}.")).await
}

#[admin_command]
pub(super) async fn plugins_reload(&self, name: String) -> Result {
	self.services.plugins.load(&name).await?;
	self.write_str(&format!("Reloaded {name}.")).await
}

#[admin_command]
pub(super) async fn plugins_call(&self, name: String, args: Vec<String>) -> Result {
	let output = self.services.plugins.command(&name, args).await?;
	self.write_str(&output).await
}
