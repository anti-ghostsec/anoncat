use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn cleanup(&self) -> Result {
	let report = self.services.cleanup.run().await?;

	self.write_str(&format!("{report:#?}")).await
}
