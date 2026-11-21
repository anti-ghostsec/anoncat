use std::fmt::Write;

use axum::extract::State;
use ruma::{
	DeviceId, MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
	api::client::{
		account::register::{self, LoginType, RegistrationKind},
		uiaa::{AuthData, AuthFlow, AuthType, LoginTermsParams, UiaaInfo},
	},
	api::error::{ErrorKind, StandardErrorBody},
	thirdparty::Medium,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{json, value::{RawValue, to_raw_value}};
use tuwunel_core::{Err, Error, Result, debug_info, debug_warn, info, utils, warn};
use tuwunel_service::{
	threepid::Association,
	users::{Register, device::generate_refresh_token},
};

use super::{SESSION_ID_LENGTH, is_matrix_appservice_irc};
use crate::Ruma;

const RANDOM_USER_ID_LENGTH: usize = 10;

/// # `POST /_matrix/client/v3/register`
///
/// Register an account on this homeserver.
///
/// You can use [`GET
/// /_matrix/client/v3/register/available`](fn.get_register_available_route.
/// html) to check if the user id is valid and available.
///
/// - Only works if registration is enabled
/// - If type is guest: ignores all parameters except
///   initial_device_display_name
/// - If sender is not appservice: Requires UIAA (but we only use a dummy stage)
/// - If type is not guest and no username is given: Always fails after UIAA
///   check
/// - Creates a new account and populates it with default account data
/// - If `inhibit_login` is false: Creates a device and returns device id and
///   access_token
#[expect(clippy::doc_markdown)]
#[tracing::instrument(skip_all, name = "register")]
pub(crate) async fn register_route(
	State(services): State<crate::State>,
	body: Ruma<register::v3::Request>,
) -> Result<register::v3::Response> {
	let is_guest = body.kind == RegistrationKind::Guest;
	let emergency_mode_enabled = services.config.emergency_password.is_some();

	gate_registration_allowed(services, &body, is_guest)?;

	// MSC4190: an appservice managing its own devices must register with
	// inhibit_login set; it cannot mint a login session via /register.
	if !body.inhibit_login
		&& body
			.appservice_info
			.as_ref()
			.is_some_and(|appservice| appservice.registration.device_management)
	{
		return Err!(Request(AppserviceLoginUnsupported(
			"Appservice has MSC4190 device management enabled; inhibit_login must be true."
		)));
	}

	let user_id =
		resolve_registration_user_id(services, &body, is_guest, emergency_mode_enabled).await?;

	check_appservice_namespace(services, &body, &user_id, emergency_mode_enabled).await?;

	let device_id = body
		.device_id
		.as_deref()
		.filter(|_| !is_guest && !body.inhibit_login);

	// Checked before anything is stored, so a refused ID leaves the name free.
	services
		.users
		.check_device_id(&user_id, device_id)
		.await?;

	let (email_association, by_captcha) = enforce_uiaa(services, &body, is_guest).await?;

	let password = if is_guest { None } else { body.password.as_deref() };

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password,
			is_appservice: body.appservice_info.is_some(),
			is_guest,
			// Not by captcha: on a fresh install, whoever solved the first
			// one would be admin. The first account made with the
			// registration token is.
			grant_first_user_admin: !by_captcha,
			..Default::default()
		})
		.await?;

	bind_registration_email(services, &user_id, email_association.as_ref()).await;

	record_accepted_terms(services, &user_id, &body, is_guest).await?;

	if (!is_guest && body.inhibit_login)
		|| body
			.appservice_info
			.as_ref()
			.is_some_and(|appservice| appservice.registration.device_management)
	{
		return Ok(register::v3::Response {
			user_id,
			device_id: None,
			access_token: None,
			refresh_token: None,
			expires_in: None,
		});
	}

	// Generate new token for the device
	let (access_token, expires_in) = services
		.users
		.generate_access_token(body.refresh_token);

	// Generate a new refresh_token if requested by client
	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	// Create device for this account
	let device_id = services
		.users
		.create_device(
			&user_id,
			device_id,
			(Some(&access_token), expires_in),
			refresh_token.as_deref(),
			body.initial_device_display_name.as_deref(),
		)
		.await?;

	debug_info!(%user_id, %device_id, "User account was created");

	if body.appservice_info.is_none() && (!is_guest || services.config.log_guest_registrations) {
		announce_new_user(services, &user_id, is_guest).await?;
	}

	Ok(register::v3::Response {
		user_id,
		device_id: Some(device_id),
		access_token: Some(access_token),
		refresh_token,
		expires_in,
	})
}

fn gate_registration_allowed(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result {
	let user = body.username.as_deref().unwrap_or("");
	let device_name = body
		.initial_device_display_name
		.as_deref()
		.unwrap_or("");

	if !services.config.allow_registration && body.appservice_info.is_none() {
		info!(
			%is_guest,
			%user,
			%device_name,
			"Rejecting registration attempt as registration is disabled"
		);

		return Err!(Request(Forbidden("Registration has been disabled.")));
	}

	if is_guest && !services.config.allow_guest_registration {
		debug_warn!(
			%device_name,
			"Guest registration disabled, rejecting guest registration attempt"
		);

		return Err!(Request(GuestAccessForbidden("Guest registration is disabled.")));
	}

	Ok(())
}

async fn resolve_registration_user_id(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
	emergency_mode_enabled: bool,
) -> Result<OwnedUserId> {
	let (Some(username), false) = (body.username.as_ref(), is_guest) else {
		loop {
			let proposed_user_id = UserId::parse_with_server_name(
				utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
				services.globals.server_name(),
			)?;

			if !services.users.exists(&proposed_user_id).await {
				return Ok(proposed_user_id);
			}
		}
	};

	let is_irc = is_matrix_appservice_irc(body.appservice_info.as_ref());

	if services
		.config
		.forbidden_usernames
		.is_match(username)
		&& !emergency_mode_enabled
	{
		return Err!(Request(Forbidden("Username is forbidden")));
	}

	// don't force the username lowercase if it's from matrix-appservice-irc
	let body_username = if is_irc {
		username.clone()
	} else {
		username.to_lowercase()
	};

	let proposed_user_id =
		match UserId::parse_with_server_name(&body_username, services.globals.server_name()) {
			| Ok(user_id) => {
				if let Err(e) = user_id.validate_strict() {
					// unless the username is from the broken matrix appservice IRC bridge, or
					// we are in emergency mode, we should follow synapse's behaviour on
					// not allowing things like spaces and UTF-8 characters in usernames
					if !is_irc && !emergency_mode_enabled {
						return Err!(Request(InvalidUsername(debug_warn!(
							"Username {body_username} contains disallowed characters or spaces: \
							 {e}"
						))));
					}
				}

				user_id
			},
			| Err(e) => {
				return Err!(Request(InvalidUsername(debug_warn!(
					"Username {body_username} is not valid: {e}"
				))));
			},
		};

	if services.users.exists(&proposed_user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	services
		.users
		.check_creation(&proposed_user_id)
		.await?;

	Ok(proposed_user_id)
}

async fn check_appservice_namespace(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	user_id: &UserId,
	emergency_mode_enabled: bool,
) -> Result {
	if body.body.login_type == Some(LoginType::ApplicationService) {
		match body.appservice_info {
			| Some(ref info) =>
				if !info.is_user_match(user_id) && !emergency_mode_enabled {
					return Err!(Request(Exclusive(
						"Username is not in an appservice namespace."
					)));
				},
			| _ => {
				return Err!(Request(MissingToken("Missing appservice token.")));
			},
		}
	} else if services
		.appservice
		.is_exclusive_user_id(user_id)
		.await
		&& !emergency_mode_enabled
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice.")));
	}

	Ok(())
}

/// anoncat's sign-up captcha, as a step apps can take in place of the
/// registration token: the picture comes in the reply's `params`, the answer
/// goes back as `response`, and `/_matrix/client/v3/auth/org.anoncat.captcha/
/// fallback/web` shows the same captcha in a browser for apps that can't.
pub(crate) const CAPTCHA_STAGE: &str = "org.anoncat.captcha";

pub(crate) fn captcha_stage() -> AuthType { AuthType::from(CAPTCHA_STAGE) }

/// Runs the sign-up steps. Also says whether the captcha was the way in.
async fn enforce_uiaa(
	services: crate::State,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result<(Option<Association>, bool)> {
	if body.appservice_info.is_some() || is_guest {
		return Ok((None, false));
	}

	let token_required = services.registration_tokens.is_enabled().await;
	let terms = services.config.login_terms_params();

	let smtp = &services.config.smtp;
	let email_required = smtp.connection_uri.is_some()
		&& (smtp.require_email_for_registration
			|| (token_required && smtp.require_email_for_token_registration));

	let stages: Vec<AuthType> = [
		token_required.then_some(AuthType::RegistrationToken),
		email_required.then_some(AuthType::EmailIdentity),
		terms.is_some().then_some(AuthType::Terms),
	]
	.into_iter()
	.flatten()
	.collect();

	// A dummy stage still forces the client through UIA when nothing else does.
	let stages = if stages.is_empty() {
		vec![AuthType::Dummy]
	} else {
		stages
	};

	// The captcha stands in for the token where the sign-up page is on.
	let captcha_offered = token_required && !email_required && services.config.web_signup;
	let captcha_flow = AuthFlow {
		stages: stages
			.iter()
			.map(|stage| match stage {
				| AuthType::RegistrationToken => captcha_stage(),
				| stage => stage.clone(),
			})
			.collect(),
	};

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow { stages }],
		completed: Vec::new(),
		params: uiaa_params(terms.as_ref(), None)?,
		session: None,
		auth_error: None,
	};

	let server_user = UserId::parse_with_server_name("", services.globals.server_name())?;
	let server_device: &DeviceId = "".into();

	match &body.auth {
		| Some(auth) if auth.auth_type() == Some(captcha_stage()) => {
			let Some(session) = auth.session() else {
				return Err!(Request(MissingParam("The captcha needs the sign-up session.")));
			};

			let mut uiaainfo = services
				.uiaa
				.get_uiaa_session(&server_user, server_device, session)
				.await?;

			if !uiaainfo
				.flows
				.iter()
				.any(|flow| flow.stages.contains(&captcha_stage()))
			{
				return Err!(Request(Forbidden("This sign-up doesn't use the captcha.")));
			}

			solve_captcha(services, auth, session, terms.as_ref(), &mut uiaainfo)?;

			if !flow_done(&uiaainfo) {
				services
					.uiaa
					.update_uiaa_session(&server_user, server_device, session, Some(&uiaainfo));

				return Err(Error::Uiaa(uiaainfo));
			}

			finish_captcha_signup(services)?;
			services
				.uiaa
				.update_uiaa_session(&server_user, server_device, session, None);

			Ok((None, true))
		},
		| Some(auth) => {
			let (worked, uiaainfo) = match email_required {
				| true =>
					services
						.uiaa
						.try_auth_registration(&server_user, server_device, auth, &uiaainfo)
						.await?,
				| false =>
					services
						.uiaa
						.try_auth(&server_user, server_device, auth, &uiaainfo)
						.await?,
			};

			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}

			// The captcha came first, then a later step (the rules) finished.
			let by_captcha = uiaainfo.completed.contains(&captcha_stage());
			if by_captcha {
				finish_captcha_signup(services)?;
			}

			let session = uiaainfo.session.expect("session is always set");
			let claim = (server_user, server_device.to_owned(), session.into());

			let association = match email_required {
				| false => None,
				| true => {
					let association = match services.threepid.redeem_claim(&claim).await {
						| Ok(association) => association,
						| Err(error)
							if error.is_not_found() || matches!(&error, Error::Request(..)) =>
						{
							return Err!(Request(Forbidden("Invalid email identity proof.")));
						},
						| Err(error) => return Err(error),
					};

					if association.medium != Medium::Email {
						return Err!(Request(Forbidden("Invalid email identity proof.")));
					}

					Some(association)
				},
			};

			services
				.uiaa
				.update_uiaa_session(&claim.0, &claim.1, &claim.2, None);

			Ok((association, by_captcha))
		},
		| _ => match body.json_body {
			| None => Err!(Request(NotJson("JSON body is not valid"))),
			| Some(ref json) => {
				let session = utils::random_string(SESSION_ID_LENGTH);

				// A picture for this session only; none left this hour, and
				// the token is the only way in for now.
				if captcha_offered
					&& let Some(png) = services
						.signup
						.captcha_for(&session, services.config.signup_captchas_per_hour)
				{
					uiaainfo.flows.push(captcha_flow);
					uiaainfo.params = uiaa_params(terms.as_ref(), Some(&png))?;
				}

				uiaainfo.session = Some(session);
				services
					.uiaa
					.create(&server_user, server_device, &uiaainfo, json);

				Err(Error::Uiaa(uiaainfo))
			},
		},
	}
}

/// Checks the captcha answer in `auth`, or, with none, that the fallback
/// page saw it solved. A wrong answer used up the picture: a new one goes in
/// the reply, with the error.
fn solve_captcha(
	services: crate::State,
	auth: &AuthData,
	session: &str,
	terms: Option<&LoginTermsParams>,
	uiaainfo: &mut UiaaInfo,
) -> Result {
	let stage = captcha_stage();
	if uiaainfo.completed.contains(&stage) {
		uiaainfo.auth_error = None;
		return Ok(());
	}

	let data = auth.data();
	let solved = data
		.get("response")
		.and_then(|response| response.as_str())
		.is_some_and(|response| services.signup.solve_for(session, response));

	if solved {
		uiaainfo.completed.push(stage);
		uiaainfo.auth_error = None;
		return Ok(());
	}

	let png = services
		.signup
		.captcha_for(session, services.config.signup_captchas_per_hour);

	let message = match (&png, data.contains_key("response")) {
		| (None, _) => "Too many people are signing up right now. Try again in a while.",
		| (Some(_), true) => "The letters didn't match the picture. Here's a new one to try.",
		| (Some(_), false) => "The captcha isn't solved yet. Here's a new picture.",
	};

	uiaainfo.params = uiaa_params(terms, png.as_deref())?;
	uiaainfo.auth_error = Some(Box::new(StandardErrorBody {
		kind: ErrorKind::forbidden(),
		message: message.to_owned(),
	}));

	let server_user = UserId::parse_with_server_name("", services.globals.server_name())?;
	services
		.uiaa
		.update_uiaa_session(&server_user, "".into(), session, Some(uiaainfo));

	Err(Error::Uiaa(uiaainfo.clone()))
}

/// Whether every step of some flow is done.
fn flow_done(uiaainfo: &UiaaInfo) -> bool {
	uiaainfo.flows.iter().any(|flow| {
		flow.stages
			.iter()
			.all(|stage| uiaainfo.completed.contains(stage))
	})
}

/// A captcha sign-up counts against the hour, like the sign-up page's.
fn finish_captcha_signup(services: crate::State) -> Result {
	if !services
		.signup
		.allow_signup(services.config.web_signups_per_hour)
	{
		return Err!(Request(Forbidden(
			"Too many accounts were made in the last hour. Please try again later."
		)));
	}

	Ok(())
}

/// The reply's `params`: the rules to accept, and the captcha's picture.
fn uiaa_params(
	terms: Option<&LoginTermsParams>,
	captcha: Option<&[u8]>,
) -> Result<Option<Box<RawValue>>> {
	let mut params = serde_json::Map::new();
	if let Some(terms) = terms {
		params.insert("m.login.terms".to_owned(), serde_json::to_value(terms)?);
	}

	if let Some(png) = captcha {
		params.insert(
			CAPTCHA_STAGE.to_owned(),
			json!({
				"image": format!("data:image/png;base64,{}", STANDARD.encode(png)),
				"expires_in": tuwunel_service::signup::APP_CAPTCHA_TTL.as_secs(),
			}),
		);
	}

	if params.is_empty() {
		return Ok(None);
	}

	Ok(Some(to_raw_value(&params)?))
}

async fn record_accepted_terms(
	services: crate::State,
	user_id: &UserId,
	body: &Ruma<register::v3::Request>,
	is_guest: bool,
) -> Result {
	if is_guest || body.appservice_info.is_some() {
		return Ok(());
	}

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

/// Bind the association spent before account creation.
///
/// Binding remains best effort after `full_register`; ownership of the proof
/// cannot be replayed even when the binding write fails.
async fn bind_registration_email(
	services: crate::State,
	user_id: &UserId,
	association: Option<&Association>,
) {
	if !services.sendmail.is_enabled() {
		return;
	}

	let Some(association) = association else {
		return;
	};

	if let Err(e) = try_bind_registration_email(services, user_id, association).await {
		warn!(%user_id, "Skipping registration email binding: {e}");
	}
}

async fn try_bind_registration_email(
	services: crate::State,
	user_id: &UserId,
	association: &Association,
) -> Result {
	if services
		.threepid
		.user_id_for_email(&association.address)
		.await?
		.is_some_and(|bound| bound != user_id)
	{
		warn!(%user_id, "Skipping registration email binding: address bound to another user");

		return Ok(());
	}

	let now = MilliSecondsSinceUnixEpoch::now();

	services
		.threepid
		.put_binding(user_id, &association.address, Medium::Email, now, now)
		.await;

	Ok(())
}

async fn announce_new_user(
	services: crate::State,
	user_id: &UserId,
	is_guest: bool,
) -> Result {
	let mut notice = String::from(if is_guest { "New guest user" } else { "New user" });

	// No device name: it names the person's browser and OS.
	write!(notice, " \"{user_id}\" registered on this server")?;

	if is_guest {
		debug_info!("{notice}");
	} else {
		info!("{notice}");
	}

	services.admin.notify(&notice).await;

	Ok(())
}
