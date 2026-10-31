use ruma::api::federation::discovery::get_server_version;
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_matrix/federation/v1/version`
///
/// Answers without naming the software or its version: anoncat doesn't
/// advertise what it runs, which would help anyone looking for a known flaw.
pub(crate) async fn get_server_version_route(
	_body: Ruma<get_server_version::v1::Request>,
) -> Result<get_server_version::v1::Response> {
	Ok(get_server_version::v1::Response { server: None })
}
