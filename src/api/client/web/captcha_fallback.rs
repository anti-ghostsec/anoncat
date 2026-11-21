//! `/_matrix/client/v3/auth/org.anoncat.captcha/fallback/web?session=…`: the
//! sign-up captcha in a browser, for apps that can't show it themselves. The
//! app opens this page, the person solves the captcha, and the app then
//! sends the step with only its session: the step is already done.

use axum::{
	extract::{Form, State},
	response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{HeaderMap, StatusCode, Uri};
use serde::Deserialize;
use tuwunel_core::utils::html::escape as html_escape;
use tuwunel_service::Services;

use super::{enabled, forbidden, notice, page, same_origin};
use crate::register::{CAPTCHA_STAGE, captcha_stage};

#[derive(Debug, Default, Deserialize)]
pub(super) struct Submit {
	#[serde(default)]
	session: String,
	#[serde(default)]
	answer: String,
}

pub(super) async fn get_route(
	State(services): State<crate::State>,
	uri: Uri,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	let session = uri
		.query()
		.unwrap_or_default()
		.split('&')
		.filter_map(|pair| pair.split_once('='))
		.find(|(key, _)| *key == "session")
		.map(|(_, value)| value)
		.unwrap_or_default();

	show(&services, session, "").await
}

pub(super) async fn post_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	uri: Uri,
	Form(body): Form<Submit>,
) -> Response {
	if let Some(off) = enabled(&services) {
		return off;
	}

	if !same_origin(&headers, &uri) {
		return forbidden(&services);
	}

	let Some((user, device, mut uiaainfo)) = signup_session(&services, &body.session).await
	else {
		return gone(&services);
	};

	if !uiaainfo.completed.contains(&captcha_stage()) {
		if !services.signup.solve_for(&body.session, &body.answer) {
			return show(
				&services,
				&body.session,
				"The letters didn't match the picture. Here's a new one to try.",
			)
			.await;
		}

		uiaainfo.completed.push(captcha_stage());
		uiaainfo.auth_error = None;
		services
			.uiaa
			.update_uiaa_session(&user, &device, &body.session, Some(&uiaainfo));
	}

	done(&services)
}

/// The sign-up session the page is for, if it's waiting on the captcha.
async fn signup_session(
	services: &Services,
	session: &str,
) -> Option<(ruma::OwnedUserId, ruma::OwnedDeviceId, ruma::api::client::uiaa::UiaaInfo)> {
	if session.is_empty() {
		return None;
	}

	services
		.uiaa
		.get_uiaa_session_by_session_id(session)
		.await
		.filter(|(user, _, uiaainfo)| {
			user.localpart().is_empty()
				&& uiaainfo
					.flows
					.iter()
					.any(|flow| flow.stages.contains(&captcha_stage()))
		})
}

async fn show(services: &Services, session: &str, error: &str) -> Response {
	let Some((_, _, uiaainfo)) = signup_session(services, session).await else {
		return gone(services);
	};

	if uiaainfo.completed.contains(&captcha_stage()) {
		return done(services);
	}

	let Some(png) = services
		.signup
		.captcha_for(session, services.config.signup_captchas_per_hour)
	else {
		return page(
			services,
			StatusCode::TOO_MANY_REQUESTS,
			"Captcha",
			"<h1>Captcha</h1><p class=\"err\" role=\"status\">Too many people are signing up \
			 right now. Please try again in a while.</p>",
		);
	};

	let picture = STANDARD.encode(&png);
	let session = html_escape(session);
	let error = notice("err", error);
	let action = format!("/_matrix/client/v3/auth/{CAPTCHA_STAGE}/fallback/web");
	let body = format!(
		r#"<h1>Captcha</h1>
<p class="lead">Your app is signing you up and needs you to read this.</p>
{error}
<form method="post" action="{action}">
<fieldset>
<legend>Type the letters in the picture</legend>
<img class="captcha" src="data:image/png;base64,{picture}" width="260" height="90" alt="A picture of letters to type, for telling people from bots">
<input name="answer" autocomplete="off" autocapitalize="characters" spellcheck="false" maxlength="12" required autofocus aria-label="Letters in the picture">
<input type="hidden" name="session" value="{session}">
<p class="hint">Capitals or not doesn't matter. Can't read it? Reload the page for another picture.</p>
</fieldset>
<button type="submit">Done</button>
</form>"#
	);

	page(services, StatusCode::OK, "Captcha", &body)
}

fn done(services: &Services) -> Response {
	page(
		services,
		StatusCode::OK,
		"Captcha solved",
		"<h1>Captcha solved</h1><p>Go back to your app and continue signing up. You can close \
		 this page.</p>",
	)
}

fn gone(services: &Services) -> Response {
	page(
		services,
		StatusCode::NOT_FOUND,
		"Captcha",
		"<h1>Captcha</h1><p class=\"err\" role=\"status\">This sign-up has ended or never \
		 started. Start again in your app.</p>",
	)
}
