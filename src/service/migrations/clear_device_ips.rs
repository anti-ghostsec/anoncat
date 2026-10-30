use ruma::api::client::device::Device;
use tuwunel_core::{
	Result, info,
	utils::{ReadyExt, stream::TryExpect},
};

use crate::Services;

pub(super) const MARKER: &str = "anoncat_clear_device_ips";

/// Clears the client IP address that older versions stored with every device.
///
/// anoncat no longer records where users connect from; this removes the
/// addresses already stored, then compacts so they leave the disk too.
pub(super) async fn clear_device_ips(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let metadata = db["userdeviceid_metadata"].clone();

	let cleared = metadata
		.raw_stream()
		.expect_ok()
		.ready_fold(0_usize, |cleared, (key, val)| {
			let Ok(mut device) = serde_json::from_slice::<Device>(val) else {
				return cleared;
			};

			if device.last_seen_ip.take().is_none() {
				return cleared;
			}

			let val = serde_json::to_vec(&device).expect("device serializes");
			metadata.insert(key, val);

			cleared.saturating_add(1)
		})
		.await;

	drop(cork);
	info!(cleared, "Cleared stored client addresses from devices");

	metadata.compact_blocking(Default::default())
}
