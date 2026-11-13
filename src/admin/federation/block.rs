use tuwunel_core::Result;

use crate::admin_command;

/// Blocks a person (`@name:server`) or a whole server, as the panel's
/// Block button does. A local account is locked too.
#[admin_command]
pub(super) async fn block(&self, target: String, reason: Vec<String>) -> Result {
	let by = self
		.sender
		.unwrap_or(&self.services.globals.server_user);
	let blocked = self
		.services
		.moderation
		.block(&target, &reason.join(" "), by, true)
		.await?;

	self.write_str(&format!("Blocked {} ({}).", blocked.target, blocked.kind))
		.await
}
