//! `/login` and the account page behind it: display name, devices, password,
//! and deleting the account. The password is checked by the same code (and
//! the same per-account throttle) as signing in from an app.

use std::{
	fmt::Write,
	time::{Duration, UNIX_EPOCH},
};

use axum::{
	extract::{Form, Path, State},
	response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::CookieJar;
use futures::{FutureExt, StreamExt};
use http::{HeaderMap, StatusCode, Uri};
use ruma::{OwnedDeviceId, OwnedUserId, UserId};
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, err, info,
	utils::{self, hash::verify_password, html::escape as html_escape},
};
use tuwunel_service::{Services, signup::Session, users::is_password_hash};

use super::{
	enabled, forbidden, notice, page, reason, removal_cookie, same_origin, see_other, session,
	session_cookie,
};
use crate::session::password_login;

const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 512;

#[derive(Debug, Default, Deserialize)]
pub(super) struct Login {
	#[serde(default)]
	username: String,
	#[serde(default)]
	password: String,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Done {
	done: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Action {
	#[serde(default)]
	csrf: String,
	#[serde(default)]
	displayname: String,
	#[serde(default)]
	device_id: String,
	#[serde(default)]
	current: String,
	#[serde(default)]
	password: String,
	#[serde(default)]
	confirm: String,
	#[serde(default)]
	logout_devices: Option<String>,
	#[serde(default)]
	erase: Option<String>,
}

pub(super) async fn login_get_route(
	State(services): State<crate::State>,
	jar: CookieJar,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	if session(&services, &jar).is_some() {
		return see_other("/account");
	}

	login_form(&services, StatusCode::OK, "", "")
}

pub(super) async fn login_post_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
	jar: CookieJar,
	Form(body): Form<Login>,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	if !same_origin(&headers, &uri) {
		return forbidden(&services);
	}

	match login(&services, &body).await {
		| Ok(user_id) => {
			let token = services.signup.start_session(&user_id);
			(jar.add(session_cookie(token)), see_other("/account")).into_response()
		},
		| Err(e) =>
			login_form(&services, e.status_code(), &body.username, &reason(&e)),
	}
}

async fn login(services: &Services, body: &Login) -> Result<OwnedUserId> {
	let wrong = || err!(Request(Forbidden("Wrong username or password.")));
	let server_name = &services.config.server_name;
	let username = body.username.trim();
	let user_id = UserId::parse_with_server_name(username, server_name).map_err(|_| wrong())?;
	let lowercased =
		UserId::parse_with_server_name(user_id.localpart().to_lowercase(), server_name)
			.map_err(|_| wrong())?;

	if !services.globals.user_is_local(&user_id) {
		return Err(wrong());
	}

	let user_id = password_login(services, &user_id, &lowercased, &body.password).await?;
	services.users.locked_check(&user_id).await?;

	Ok(user_id)
}

fn login_form(services: &Services, status: StatusCode, username: &str, error: &str) -> Response {
	let username = html_escape(username);
	let error = notice("err", error);
	let signup = services
		.config
		.allow_registration
		.then_some(r#"<p class="foot">No account yet? <a href="/register">Create one</a></p>"#)
		.unwrap_or_default();

	let body = format!(
		r#"<h1>Sign in</h1>
<p>Manage your account: display name, devices, password.</p>
{error}
<form method="post" action="/login">
<label for="username">Username</label>
<input id="username" name="username" value="{username}" autocomplete="username" autocapitalize="none" spellcheck="false" required autofocus>
<label for="password">Password</label>
<input id="password" name="password" type="password" autocomplete="current-password" required>
<button type="submit">Sign in</button>
</form>
{signup}"#
	);

	page(services, status, "Sign in", &body)
}

pub(super) async fn get_route(
	State(services): State<crate::State>,
	jar: CookieJar,
	uri: Uri,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	let Some(session) = session(&services, &jar) else {
		return see_other("/login");
	};

	let query: Done = serde_html_form::from_str(uri.query().unwrap_or_default()).unwrap_or_default();
	let done = match query.done.as_deref() {
		| Some("name") => "Display name saved.",
		| Some("device") => "That device is signed out.",
		| Some("password") => "Password changed.",
		| _ => "",
	};

	account_page(&services, &session, StatusCode::OK, done, "").await
}

pub(super) async fn post_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
	jar: CookieJar,
	Path(action): Path<String>,
	Form(body): Form<Action>,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	let Some(session) = session(&services, &jar) else {
		return see_other("/login");
	};

	if !same_origin(&headers, &uri) || body.csrf != session.csrf {
		return forbidden(&services);
	}

	let user_id = &session.user_id;
	let result = match action.as_str() {
		| "logout" => {
			if let Some(cookie) = jar.get(super::COOKIE) {
				services.signup.end_session(cookie.value());
			}

			return (jar.remove(removal_cookie()), see_other("/login")).into_response();
		},
		| "name" => set_name(&services, user_id, &body).await.map(|()| "name"),
		| "device" => sign_out_device(&services, user_id, &body)
			.await
			.map(|()| "device"),
		| "password" => match change_password(&services, user_id, &body).await {
			| Ok(()) => {
				// Every other browser signed in here is signed out, this one
				// keeps going under a new cookie.
				services.signup.end_sessions_for(user_id);
				let token = services.signup.start_session(user_id);
				let jar = jar.add(session_cookie(token));

				return (jar, see_other("/account?done=password")).into_response();
			},
			| Err(e) => Err(e),
		},
		| "delete" => match delete(&services, user_id, &body).await {
			| Ok(()) => {
				services.signup.end_sessions_for(user_id);
				let body = "<h1>Account deleted</h1><p>Your account is gone and can't be \
				            signed in to again. Its username can't be used again either.</p>";

				let page = page(&services, StatusCode::OK, "Account deleted", body);
				return (jar.remove(removal_cookie()), page).into_response();
			},
			| Err(e) => Err(e),
		},
		| _ => return StatusCode::NOT_FOUND.into_response(),
	};

	match result {
		| Ok(done) => see_other(&format!("/account?done={done}")),
		| Err(e) =>
			account_page(&services, &session, e.status_code(), "", &reason(&e)).await,
	}
}

async fn set_name(services: &Services, user_id: &UserId, body: &Action) -> Result {
	let name: String = body
		.displayname
		.trim()
		.chars()
		.filter(|c| !c.is_control())
		.take(255)
		.collect();

	let name = (!name.is_empty()).then_some(name.as_str());
	services
		.profile
		.set_displayname(user_id, name, None)
		.await
}

async fn sign_out_device(services: &Services, user_id: &UserId, body: &Action) -> Result {
	let device_id: OwnedDeviceId = body.device_id.as_str().into();
	if !services
		.users
		.device_exists(user_id, &device_id)
		.await
	{
		return Err!(Request(NotFound("That device isn't signed in.")));
	}

	services
		.users
		.remove_device(user_id, &device_id)
		.await;

	Ok(())
}

async fn change_password(services: &Services, user_id: &UserId, body: &Action) -> Result {
	confirm_password(services, user_id, &body.current, "Your current password isn't right.")
		.await?;

	if body.password != body.confirm {
		return Err!(Request(InvalidParam("The two new passwords aren't the same.")));
	}

	let length = body.password.chars().count();
	if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
		return Err!(Request(InvalidParam(
			"The new password needs at least {PASSWORD_MIN} characters (and at most \
			 {PASSWORD_MAX})."
		)));
	}

	services
		.users
		.set_password_if_active(user_id, Some(&body.password))
		.await?;

	if body.logout_devices.as_deref() == Some("on") {
		let devices: Vec<OwnedDeviceId> = services
			.users
			.all_device_ids(user_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for device_id in devices {
			services
				.users
				.remove_device(user_id, &device_id)
				.await;
		}
	}

	info!("User {user_id} changed their password.");
	services
		.admin
		.notify(&format!("User {user_id} changed their password."))
		.await;

	Ok(())
}

async fn delete(services: &Services, user_id: &UserId, body: &Action) -> Result {
	confirm_password(services, user_id, &body.current, "Your password isn't right.").await?;

	let typed = body.confirm.trim();
	let typed = typed.strip_prefix('@').unwrap_or(typed);
	let typed = typed.split(':').next().unwrap_or_default();
	if !typed.eq_ignore_ascii_case(user_id.localpart()) {
		return Err!(Request(InvalidParam("Type your username to confirm deleting the account.")));
	}

	let erase = body.erase.as_deref() == Some("on");
	services
		.deactivate
		.full_deactivate(user_id, erase)
		.boxed()
		.await?;

	info!("User {user_id} deactivated their account.");
	services
		.admin
		.notify(&format!("User {user_id} deactivated their account."))
		.await;

	Ok(())
}

/// Checks the password of someone already signed in to the page, as apps do
/// before a password change. It counts against the failed-attempt limit like
/// a sign-in, but a right one isn't a new sign-in.
async fn confirm_password(
	services: &Services,
	user_id: &UserId,
	password: &str,
	wrong: &'static str,
) -> Result {
	let reservation = services
		.login_ratelimit
		.reserve_login_attempt(user_id)?;

	let verified = services
		.users
		.password_hash(user_id)
		.await
		.ok()
		.filter(|hash| is_password_hash(hash))
		.map(|hash| verify_password(password, &hash).is_ok());

	if verified != Some(false) {
		services
			.login_ratelimit
			.refund_login_attempt(reservation)?;
	}

	match verified {
		| Some(true) => Ok(()),
		| _ => Err!(Request(Forbidden("{wrong}"))),
	}
}

async fn account_page(
	services: &Services,
	session: &Session,
	status: StatusCode,
	done: &str,
	error: &str,
) -> Response {
	let user_id = &session.user_id;
	let user = html_escape(user_id.as_str());
	let csrf = html_escape(&session.csrf);
	let name = services
		.profile
		.displayname(user_id)
		.await
		.unwrap_or_default();
	let name = html_escape(&name);
	let messages = notice("ok", done) + &notice("err", error);
	let devices = devices(services, user_id, &csrf).await;
	let csrf_field = format!(r#"<input type="hidden" name="csrf" value="{csrf}">"#);

	let body = format!(
		r#"<h1>Your account</h1>
<p>Signed in as <strong>{user}</strong>.</p>
<form method="post" action="/account/logout" class="inline">{csrf_field}<button type="submit" class="quiet">Sign out of this page</button></form>
{messages}
<section>
<h2>Display name</h2>
<form method="post" action="/account/name">
{csrf_field}
<label for="displayname">Shown to people you chat with</label>
<input id="displayname" name="displayname" value="{name}" maxlength="255">
<button type="submit">Save</button>
</form>
</section>
<section>
<h2>Devices</h2>
<p class="hint">Apps and browsers signed in to your account. Sign out any you don't recognise.</p>
{devices}
</section>
<section>
<h2>Change password</h2>
<form method="post" action="/account/password">
{csrf_field}
<label for="current">Current password</label>
<input id="current" name="current" type="password" autocomplete="current-password" required>
<label for="password">New password</label>
<input id="password" name="password" type="password" autocomplete="new-password" minlength="{PASSWORD_MIN}" maxlength="{PASSWORD_MAX}" required>
<label for="confirm">New password again</label>
<input id="confirm" name="confirm" type="password" autocomplete="new-password" minlength="{PASSWORD_MIN}" maxlength="{PASSWORD_MAX}" required>
<label class="check"><input type="checkbox" name="logout_devices" value="on"> Also sign out all my devices</label>
<button type="submit">Change password</button>
</form>
</section>
<section class="danger">
<h2>Delete account</h2>
<p>This can't be undone. You leave every room, your username can't be used again, and you lose access to your encrypted messages.</p>
<form method="post" action="/account/delete">
{csrf_field}
<label for="del-current">Password</label>
<input id="del-current" name="current" type="password" autocomplete="current-password" required>
<label for="del-confirm">Type your username to confirm</label>
<input id="del-confirm" name="confirm" autocomplete="off" autocapitalize="none" spellcheck="false" required>
<label class="check"><input type="checkbox" name="erase" value="on" checked> Also erase my profile and account data</label>
<button type="submit" class="danger">Delete my account</button>
</form>
</section>"#
	);

	page(services, status, "Your account", &body)
}

async fn devices(services: &Services, user_id: &UserId, csrf: &str) -> String {
	let mut devices: Vec<_> = services
		.users
		.all_devices_metadata(user_id)
		.collect()
		.await;

	if devices.is_empty() {
		return r#"<p class="hint">None right now.</p>"#.to_owned();
	}

	devices.sort_by_key(|device| std::cmp::Reverse(device.last_seen_ts));

	let rows = devices
		.iter()
		.fold(String::new(), |mut rows, device| {
			let id = html_escape(device.device_id.as_str());
			let name = html_escape(device.display_name.as_deref().unwrap_or("Unnamed device"));
			let seen = device
				.last_seen_ts
				.map(|ts| {
					let at = UNIX_EPOCH + Duration::from_millis(u64::from(ts.0));
					utils::time::format(at, "%Y-%m-%d %H:%M UTC")
				})
				.unwrap_or_else(|| "—".to_owned());

			write!(
				rows,
				r#"<tr><td>{name}<br><code>{id}</code></td><td>{seen}</td><td><form method="post" action="/account/device"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="device_id" value="{id}"><button type="submit" class="quiet">Sign out</button></form></td></tr>"#
			)
			.ok();

			rows
		});

	format!(
		r#"<table><thead><tr><th>Device</th><th>Last active</th><th></th></tr></thead><tbody>{rows}</tbody></table>"#
	)
}
