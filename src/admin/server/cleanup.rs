use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn cleanup(&self) -> Result {
	let report = self.services.cleanup.run("command").await?;

	self.write_str(&format!("{report:#?}")).await
}
