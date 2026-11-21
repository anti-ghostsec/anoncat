//! State behind anoncat's sign-up captcha (`web_signup`): the captchas handed
//! out and not yet answered, on the web page and in apps, how many were
//! handed out and how many accounts were made in the last hour, and who is
//! signed in to the account page. All of it is in memory only; a restart
//! forgets it, which only signs people out of the page.

mod captcha;

use std::{
	collections::{HashMap, VecDeque},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use ruma::{OwnedUserId, UserId};
use tuwunel_core::{Result, utils};

/// How long a captcha can be answered.
const CAPTCHA_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a captcha shown in an app can be answered.
pub const APP_CAPTCHA_TTL: Duration = Duration::from_secs(5 * 60);
/// Captchas kept at once; past it the oldest go first.
const CAPTCHAS_MAX: usize = 4096;
/// How long the account page stays signed in without being used.
const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);
/// How long it stays signed in at most.
const SESSION_MAX: Duration = Duration::from_secs(8 * 60 * 60);
const SESSIONS_MAX: usize = 4096;
const TOKEN_LENGTH: usize = 32;
const HOUR: Duration = Duration::from_secs(60 * 60);

pub struct Service {
	captchas: Mutex<HashMap<String, Pending>>,
	/// Captchas shown in apps, by the sign-up session they belong to.
	app_captchas: Mutex<HashMap<String, Pending>>,
	/// When captchas were handed out, web and apps together.
	issued: Mutex<VecDeque<Instant>>,
	signups: Mutex<VecDeque<Instant>>,
	sessions: Mutex<HashMap<String, Session>>,
}

struct Pending {
	answer: String,
	made: Instant,
}

/// A browser signed in to the account page.
#[derive(Clone, Debug)]
pub struct Session {
	pub user_id: OwnedUserId,
	/// Sent back with every form, so another site can't post them.
	pub csrf: String,
	/// A mark of the account's password when the session started: once the
	/// password changes, from anywhere, the session no longer works.
	pub password: String,
	started: Instant,
	seen: Instant,
}

/// A fresh captcha: the token the form sends back, and the picture.
pub struct Captcha {
	pub token: String,
	pub png: Vec<u8>,
}

impl crate::Service for Service {
	fn build(_args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			captchas: Mutex::default(),
			app_captchas: Mutex::default(),
			issued: Mutex::default(),
			signups: Mutex::default(),
			sessions: Mutex::default(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Draws a new captcha and remembers its answer. `None` once
	/// `per_hour` captchas were handed out in the last hour (0: no limit),
	/// so asking for pictures over and over gets nowhere.
	#[must_use]
	pub fn captcha(&self, per_hour: u32) -> Option<Captcha> {
		if !within_hour(&self.issued, per_hour) {
			return None;
		}

		let answer = captcha::answer();
		let png = captcha::draw(&answer);
		let token = utils::random_string(TOKEN_LENGTH);
		let now = Instant::now();

		let mut captchas = self.captchas.lock().expect("locked");
		captchas.retain(|_, pending| now.duration_since(pending.made) < CAPTCHA_TTL);
		if captchas.len() >= CAPTCHAS_MAX {
			let oldest = captchas
				.iter()
				.min_by_key(|(_, pending)| pending.made)
				.map(|(token, _)| token.clone());

			if let Some(oldest) = oldest {
				captchas.remove(&oldest);
			}
		}

		captchas.insert(token.clone(), Pending { answer, made: now });
		Some(Captcha { token, png })
	}

	/// Draws a captcha for an app's sign-up `session`, in place of any it
	/// had: one picture per session at a time. `None` once `per_hour`
	/// captchas were handed out in the last hour (0: no limit).
	#[must_use]
	pub fn captcha_for(&self, session: &str, per_hour: u32) -> Option<Vec<u8>> {
		if !within_hour(&self.issued, per_hour) {
			return None;
		}

		let answer = captcha::answer();
		let png = captcha::draw(&answer);
		let now = Instant::now();

		let mut captchas = self.app_captchas.lock().expect("locked");
		captchas.retain(|_, pending| now.duration_since(pending.made) < APP_CAPTCHA_TTL);
		if captchas.len() >= CAPTCHAS_MAX && !captchas.contains_key(session) {
			let oldest = captchas
				.iter()
				.min_by_key(|(_, pending)| pending.made)
				.map(|(session, _)| session.clone());

			if let Some(oldest) = oldest {
				captchas.remove(&oldest);
			}
		}

		captchas.insert(session.to_owned(), Pending { answer, made: now });
		Some(png)
	}

	/// Whether `answer` solves the captcha of the sign-up `session`. Either
	/// way it's used up: one guess per picture.
	#[must_use]
	pub fn solve_for(&self, session: &str, answer: &str) -> bool {
		let Some(pending) = self
			.app_captchas
			.lock()
			.expect("locked")
			.remove(session)
		else {
			return false;
		};

		pending.made.elapsed() < APP_CAPTCHA_TTL && normalize(answer) == pending.answer
	}

	/// Whether `answer` solves the captcha `token`. Either way the captcha is
	/// used up, so each picture gets one guess.
	#[must_use]
	pub fn solve(&self, token: &str, answer: &str) -> bool {
		let Some(pending) = self
			.captchas
			.lock()
			.expect("locked")
			.remove(token)
		else {
			return false;
		};

		pending.made.elapsed() < CAPTCHA_TTL && normalize(answer) == pending.answer
	}

	/// Counts a sign-up against the hourly limit, or says it's reached.
	#[must_use]
	pub fn allow_signup(&self, per_hour: u32) -> bool { within_hour(&self.signups, per_hour) }

	/// Signs `user_id` in to the account page; the token goes in its cookie.
	#[must_use]
	pub fn start_session(&self, user_id: &UserId, password: String) -> String {
		let token = utils::random_string(TOKEN_LENGTH);
		let now = Instant::now();
		let session = Session {
			user_id: user_id.to_owned(),
			csrf: utils::random_string(TOKEN_LENGTH),
			password,
			started: now,
			seen: now,
		};

		let mut sessions = self.sessions.lock().expect("locked");
		sessions.retain(|_, session| session.live(now));
		if sessions.len() >= SESSIONS_MAX {
			let oldest = sessions
				.iter()
				.min_by_key(|(_, session)| session.seen)
				.map(|(token, _)| token.clone());

			if let Some(oldest) = oldest {
				sessions.remove(&oldest);
			}
		}

		sessions.insert(token.clone(), session);
		token
	}

	/// The signed-in session for a cookie, kept alive by being used.
	#[must_use]
	pub fn session(&self, token: &str) -> Option<Session> {
		let now = Instant::now();
		let mut sessions = self.sessions.lock().expect("locked");
		let session = sessions.get_mut(token)?;
		if !session.live(now) {
			sessions.remove(token);
			return None;
		}

		session.seen = now;
		Some(session.clone())
	}

	pub fn end_session(&self, token: &str) {
		self.sessions
			.lock()
			.expect("locked")
			.remove(token);
	}

	/// Signs `user_id` out of the account page everywhere, as after a
	/// password change or deleting the account.
	pub fn end_sessions_for(&self, user_id: &UserId) {
		self.sessions
			.lock()
			.expect("locked")
			.retain(|_, session| session.user_id != user_id);
	}
}

impl Session {
	fn live(&self, now: Instant) -> bool {
		now.duration_since(self.seen) < SESSION_IDLE
			&& now.duration_since(self.started) < SESSION_MAX
	}
}

/// Counts one more in `times`, or says `per_hour` were already counted in
/// the last hour (0: no limit).
fn within_hour(times: &Mutex<VecDeque<Instant>>, per_hour: u32) -> bool {
	let now = Instant::now();
	let mut times = times.lock().expect("locked");
	while times
		.front()
		.is_some_and(|&at| now.duration_since(at) >= HOUR)
	{
		times.pop_front();
	}

	let limit = usize::try_from(per_hour).unwrap_or(usize::MAX);
	if per_hour > 0 && times.len() >= limit {
		return false;
	}

	times.push_back(now);
	true
}

/// An answer as the captcha compares it: no spaces, capitals or not alike.
fn normalize(answer: &str) -> String {
	answer
		.chars()
		.filter(|c| !c.is_whitespace())
		.map(|c| c.to_ascii_uppercase())
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn service() -> Service {
		Service {
			captchas: Mutex::default(),
			app_captchas: Mutex::default(),
			issued: Mutex::default(),
			signups: Mutex::default(),
			sessions: Mutex::default(),
		}
	}

	#[test]
	fn captcha_takes_one_guess() {
		let service = service();
		let captcha = service.captcha(0).unwrap();
		let answer = service.captchas.lock().unwrap()[&captcha.token]
			.answer
			.clone();

		assert!(!service.solve(&captcha.token, "nope"));
		assert!(!service.solve(&captcha.token, &answer));

		let captcha = service.captcha(0).unwrap();
		let answer = service.captchas.lock().unwrap()[&captcha.token]
			.answer
			.to_lowercase();
		assert!(service.solve(&captcha.token, &format!(" {answer} ")));
	}

	#[test]
	fn app_captcha_is_per_session_and_takes_one_guess() {
		let service = service();
		assert!(service.captcha_for("s1", 0).is_some());
		let answer = service.app_captchas.lock().unwrap()["s1"].answer.clone();

		assert!(!service.solve_for("s2", &answer), "another session's picture");
		assert!(!service.solve_for("s1", "nope"));
		assert!(!service.solve_for("s1", &answer), "a wrong guess used it up");

		assert!(service.captcha_for("s1", 0).is_some());
		let answer = service.app_captchas.lock().unwrap()["s1"].answer.to_lowercase();
		assert!(service.solve_for("s1", &format!(" {answer} ")));
		assert!(!service.solve_for("s1", &answer), "a right guess used it up too");
	}

	#[test]
	fn captchas_handed_out_count_against_the_hour() {
		let service = service();
		assert!(service.captcha(2).is_some());
		assert!(service.captcha_for("s", 2).is_some());
		assert!(service.captcha(2).is_none());
		assert!(service.captcha_for("s", 2).is_none());
		assert!(service.captcha(0).is_some());
	}

	#[test]
	fn signups_stop_at_the_limit() {
		let service = service();
		assert!(service.allow_signup(2));
		assert!(service.allow_signup(2));
		assert!(!service.allow_signup(2));
		assert!(service.allow_signup(0));
	}

	#[test]
	fn sessions_end() {
		let service = service();
		let user: OwnedUserId = "@a:example.com".try_into().unwrap();
		let token = service.start_session(&user, String::new());
		assert_eq!(service.session(&token).unwrap().user_id, user);

		service.end_sessions_for(&user);
		assert!(service.session(&token).is_none());
	}
}
