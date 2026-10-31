//! `POST /_synapse/admin/v1/send_server_notice` and its transaction form.
//!
//! Server notices are sent by the server's bot account, which anoncat doesn't
//! have; both answer with an error saying so.

use axum::extract::State;
use synapse_admin_api::server_notices::send::{
	by_txn,
	v1::{self, Response},
};
use tuwunel_core::{Err, Result};

use crate::RumaAdmin;

pub(crate) async fn admin_send_server_notice_route(
	State(_services): State<crate::State>,
	_body: RumaAdmin<v1::Request>,
) -> Result<Response> {
	unavailable()
}

pub(crate) async fn admin_send_server_notice_txn_route(
	State(_services): State<crate::State>,
	_body: RumaAdmin<by_txn::Request>,
) -> Result<Response> {
	unavailable()
}

fn unavailable() -> Result<Response> {
	Err!(Request(Forbidden(
		"Server notices are off: this server has no bot account to send them."
	)))
}
