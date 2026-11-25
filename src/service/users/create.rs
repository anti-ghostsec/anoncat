use ruma::UserId;
use tuwunel_core::{Err, Result, implement, utils::hash::is_phc_hash};

use super::PASSWORD_DISABLED;

/// Creates a password account from an Argon2 hash made elsewhere (the twin
/// of an account on another server), so the same password works here. No
/// hash makes an account nobody can sign in to with a password. Refused when
/// the account exists or the hash isn't a complete Argon2 PHC string.
#[implement(super::Service)]
pub async fn create_with_hash(&self, user_id: &UserId, hash: Option<&str>) -> Result {
	if hash.is_some_and(|hash| !is_phc_hash(hash)) {
		return Err!(Request(InvalidParam("Not an Argon2 password hash.")));
	}

	let _password_guard = self.changing_password.lock(user_id).await;

	self.check_creation(user_id).await?;
	if self.exists(user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	let mut txn = self.services.db.txn();
	txn.insert_raw(&self.db.userid_origin, user_id, "password");
	txn.insert_raw(&self.db.userid_password, user_id, hash.unwrap_or(PASSWORD_DISABLED));
	txn.execute();

	Ok(())
}

/// Replaces an active account's password with an Argon2 hash made elsewhere.
/// Sessions are left as they are. Refused for a deactivated account or a hash
/// that isn't a complete Argon2 PHC string.
#[implement(super::Service)]
pub async fn set_password_hash(&self, user_id: &UserId, hash: &str) -> Result {
	if !is_phc_hash(hash) {
		return Err!(Request(InvalidParam("Not an Argon2 password hash.")));
	}

	let _password_guard = self.changing_password.lock(user_id).await;

	self.deactivated_check(user_id).await?;
	if !self.exists(user_id).await {
		return Err!(Request(NotFound("User does not exist.")));
	}

	let mut txn = self.services.db.txn();
	txn.insert_raw(&self.db.userid_password, user_id, hash);
	txn.insert_raw(&self.db.userid_origin, user_id, "password");
	txn.execute();

	Ok(())
}

#[implement(super::Service)]
pub(super) fn create_disabled(&self, user_id: &UserId, origin: &str) {
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userid_origin, user_id, origin);
	txn.insert_raw(&self.db.userid_password, user_id, PASSWORD_DISABLED);
	txn.execute();
}

#[cfg(test)]
mod tests;
