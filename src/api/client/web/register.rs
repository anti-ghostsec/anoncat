//! `/register`: anyone who reads the captcha gets an account, up to
//! `web_signups_per_hour` of them an hour. No registration token is asked;
//! the Matrix register endpoint keeps asking for one, so this page, captcha
//! and all, is the only open way in.

use std::fmt::Write;

use axum::{
	extract::{Form, State},
	response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::CookieJar;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{HeaderMap, StatusCode, Uri};
use ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{Err, Result, err, info, utils::html::escape as html_escape};
use tuwunel_service::{Services, users::Register};

use super::{enabled, forbidden, notice, page, reason, same_origin, session_cookie};

const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 512;

#[derive(Debug, Default, Deserialize)]
pub(super) struct Submit {
	#[serde(default)]
	username: String,
	#[serde(default)]
	password: String,
	#[serde(default)]
	confirm: String,
	#[serde(default)]
	captcha: String,
	#[serde(default)]
	token: String,
	#[serde(default)]
	accept_terms: Option<String>,
}

pub(super) async fn get_route(State(services): State<crate::State>) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	form(&services, StatusCode::OK, "", "")
}

pub(super) async fn post_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
	jar: CookieJar,
	Form(body): Form<Submit>,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	if !same_origin(&headers, &uri) {
		return forbidden(&services);
	}

	match register(&services, &body).await {
		| Ok(user_id) => {
			let token = services.signup.start_session(&user_id);
			let jar = jar.add(session_cookie(token));
			(jar, welcome(&services, &user_id)).into_response()
		},
		| Err(e) => form(
			&services,
			e.status_code(),
			&body.username,
			&reason(&e),
		),
	}
}

async fn register(services: &Services, body: &Submit) -> Result<OwnedUserId> {
	if !services.config.allow_registration {
		return Err!(Request(Forbidden("Sign-ups are closed on this server.")));
	}

	// First, so each picture gets one guess at everything else too.
	if !services.signup.solve(&body.token, &body.captcha) {
		return Err!(Request(Forbidden(
			"The letters didn't match the picture. Here's a new one to try."
		)));
	}

	let smtp = &services.config.smtp;
	if smtp.connection_uri.is_some() && smtp.require_email_for_registration {
		return Err!(Request(Forbidden(
			"This server needs an email address to sign up, which this page doesn't ask for."
		)));
	}

	let user_id = check_username(services, &body.username).await?;

	if body.password != body.confirm {
		return Err!(Request(InvalidParam("The two passwords aren't the same.")));
	}

	let length = body.password.chars().count();
	if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
		return Err!(Request(InvalidParam(
			"The password needs at least {PASSWORD_MIN} characters (and at most {PASSWORD_MAX})."
		)));
	}

	if !services.config.registration_terms.is_empty()
		&& body.accept_terms.as_deref() != Some("on")
	{
		return Err!(Request(Forbidden("You need to accept the terms to sign up.")));
	}

	// Last, so only real sign-ups count against the hour.
	if !services
		.signup
		.allow_signup(services.config.web_signups_per_hour)
	{
		return Err!(Request(Forbidden(
			"Too many accounts were made in the last hour. Please try again later."
		)));
	}

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(&body.password),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	record_accepted_terms(services, &user_id).await?;

	let notice = format!("New user \"{user_id}\" registered on this server (sign-up page)");
	info!("{notice}");
	services.admin.notify(&notice).await;

	Ok(user_id)
}

/// The same rules as the Matrix register endpoint.
async fn check_username(services: &Services, username: &str) -> Result<OwnedUserId> {
	let username = username.trim().to_lowercase();
	let username = username
		.strip_prefix('@')
		.unwrap_or(&username)
		.split(':')
		.next()
		.unwrap_or_default()
		.to_owned();

	if username.is_empty() {
		return Err!(Request(InvalidUsername("Pick a username.")));
	}

	if services
		.config
		.forbidden_usernames
		.is_match(&username)
	{
		return Err!(Request(Forbidden("That username isn't allowed.")));
	}

	let user_id = UserId::parse_with_server_name(&username, &services.config.server_name)
		.map_err(|_| err!(Request(InvalidUsername("That username isn't valid."))))?;

	user_id.validate_strict().map_err(|_| {
		err!(Request(InvalidUsername(
			"Usernames can only have lowercase letters, numbers and . _ = - /"
		)))
	})?;

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("That username is reserved.")));
	}

	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("That username is taken.")));
	}

	services.users.check_creation(&user_id).await?;

	Ok(user_id)
}

async fn record_accepted_terms(services: &Services, user_id: &UserId) -> Result {
	let accepted: Vec<String> = services
		.config
		.registration_terms
		.values()
		.flat_map(|policy| policy.translations.values())
		.map(|translation| translation.url.to_string())
		.collect();

	if accepted.is_empty() {
		return Ok(());
	}

	let event_type = "m.accepted_terms";
	let event = json!({
		"type": event_type,
		"content": { "accepted": accepted },
	});

	services
		.account_data
		.update(None, user_id, event_type.into(), &event)
		.await
}

fn form(services: &Services, status: StatusCode, username: &str, error: &str) -> Response {
	let captcha = services.signup.captcha();
	let picture = STANDARD.encode(&captcha.png);
	let token = html_escape(&captcha.token);
	let username = html_escape(username);
	let server = html_escape(services.config.server_name.as_str());
	let error = notice("err", error);
	let terms = terms(services);

	let body = format!(
		r#"<h1>Create an account</h1>
{error}
<form method="post" action="/register">
<label for="username">Username</label>
<input id="username" name="username" value="{username}" autocomplete="username" autocapitalize="none" spellcheck="false" maxlength="64" required autofocus>
<p class="hint">Lowercase letters, numbers and . _ = - /. Your address will be @name:{server}.</p>
<label for="password">Password</label>
<input id="password" name="password" type="password" autocomplete="new-password" minlength="{PASSWORD_MIN}" maxlength="{PASSWORD_MAX}" required>
<label for="confirm">Password again</label>
<input id="confirm" name="confirm" type="password" autocomplete="new-password" minlength="{PASSWORD_MIN}" maxlength="{PASSWORD_MAX}" required>
<fieldset>
<legend>Type the letters in the picture</legend>
<img class="captcha" src="data:image/png;base64,{picture}" width="260" height="90" alt="A picture of letters to type, for telling people from bots">
<input name="captcha" autocomplete="off" autocapitalize="characters" spellcheck="false" maxlength="12" required aria-label="Letters in the picture">
<input type="hidden" name="token" value="{token}">
<p class="hint">Capitals or not doesn't matter. Can't read it? <a href="/register">Get another picture</a>.</p>
</fieldset>
{terms}
<button type="submit">Create account</button>
</form>
<p class="foot">Already have an account? <a href="/login">Sign in</a></p>"#
	);

	page(services, status, "Create an account", &body)
}

fn terms(services: &Services) -> String {
	let policies = &services.config.registration_terms;
	if policies.is_empty() {
		return String::new();
	}

	let links = policies
		.values()
		.filter_map(|policy| {
			policy
				.translations
				.get("en")
				.or_else(|| policy.translations.values().next())
		})
		.fold(String::new(), |mut links, translation| {
			write!(
				links,
				r#"<li><a href="{}" rel="noopener noreferrer">{}</a></li>"#,
				html_escape(translation.url.as_str()),
				html_escape(&translation.name),
			)
			.ok();

			links
		});

	format!(
		r#"<fieldset><legend>Terms</legend><ul>{links}</ul><label class="check"><input type="checkbox" name="accept_terms" value="on" required> I accept the terms above.</label></fieldset>"#
	)
}

fn welcome(services: &Services, user_id: &UserId) -> Response {
	let user = html_escape(user_id.as_str());
	let server = html_escape(services.config.server_name.as_str());
	let body = format!(
		r#"<h1>Your account is ready</h1>
<p>You are <strong>{user}</strong>.</p>
<p>To chat, sign in from a Matrix app such as Element: choose another homeserver, enter <strong>{server}</strong>, then your username and password.</p>
<p>Your password can't be reset by email, so keep it somewhere safe. When an app offers to set up secure backup or a recovery key, do it: it's the only way to read your encrypted messages on a new device.</p>
<p><a class="button" href="/account">Go to your account</a></p>"#
	);

	page(services, StatusCode::OK, "Account ready", &body)
}
