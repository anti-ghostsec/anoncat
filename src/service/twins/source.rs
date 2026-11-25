//! The source side: keeping the twins of this server's accounts in step.

use std::{
	collections::{BTreeMap, HashSet},
	time::Duration,
};

use futures::{FutureExt, StreamExt};
use reqwest::{Client, Method, header::CONTENT_TYPE, redirect::Policy};
use ruma::{
	Int, OwnedRoomId, OwnedUserId, RoomId, UserId,
	events::{
		GlobalAccountDataEventType, StateEventType,
		direct::DirectEvent,
		room::{
			create::RoomCreateEventContent,
			join_rules::JoinRule,
			member::MembershipState,
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent, UserPowerLevel},
		},
	},
};
use serde::{Serialize, de::DeserializeOwned};
use tuwunel_core::{
	Result, debug, debug_info, debug_warn, implement, info,
	utils::{hash::is_phc_hash, stream::ReadyExt},
	warn,
};
use tuwunel_matrix::pdu::PduBuilder;
use url::Url;

use super::{Account, AccountResult, JoinRequest, Service, read_secret};

/// Wait after startup before the first run.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(30);

/// The power level that runs a room.
const ADMIN_LEVEL: u8 = 100;

/// Why a call to the twin side didn't work.
enum CallError {
	/// Nothing answered: the twin server is down. The run stops there.
	Unreachable,
	/// It said no to this one request (4xx); asking again won't help until
	/// something changes.
	Refused(String),
	/// It failed (5xx or a bad answer); try again next run.
	Failed(String),
}

/// The twin side's address and secret, for one run.
struct Api<'a> {
	client: &'a Client,
	base: &'a Url,
	secret: String,
}

/// What decides whether a twin belongs in a room.
#[derive(Clone, Copy, Debug, Default)]
struct RoomFacts {
	/// Listed in the person's `m.direct`.
	is_dm: bool,
	/// `federate` in the room's create event.
	federates: bool,
	/// Anyone can join.
	public: bool,
	/// The person's power level is 100 or more.
	user_is_admin: bool,
	/// The person's notices room.
	notice: bool,
}

#[implement(Service)]
pub(super) async fn source_worker(&self) -> Result {
	if !self.is_source() {
		return Ok(());
	}

	// Loopback only (checked at startup), never through a proxy.
	let client = Client::builder()
		.no_proxy()
		.redirect(Policy::none())
		.connect_timeout(Duration::from_secs(10))
		.timeout(Duration::from_mins(2))
		.build()?;

	let mut wait = FIRST_RUN_DELAY;
	loop {
		tokio::select! {
			() = tokio::time::sleep(wait) => {},
			() = self.services.server.until_shutdown() => return Ok(()),
		}

		self.sync(&client).boxed().await;
		wait = self.services.server.config.twin_sync_interval;
	}
}

/// One run: accounts first, then rooms.
#[implement(Service)]
async fn sync(&self, client: &Client) {
	let config = &self.services.server.config;
	let (Some(base), Some(path)) = (&config.twin_api, &config.twin_secret_file) else {
		return;
	};

	let secret = match read_secret(path).await {
		| Ok(secret) => secret,
		| Err(e) => {
			self.warn_quietly(&format!("Twin secret unreadable, twins not updated: {e}"));
			return;
		},
	};

	let api = Api { client, base, secret };
	if self.sync_accounts(&api).await.is_err() || self.sync_rooms(&api).await.is_err() {
		self.warn_quietly("Twin server unreachable; trying again quietly");
	}
}

/// Sends every account whose state changed since it was last sent (all of
/// them after a restart). Stops at the first unreachable request.
#[implement(Service)]
async fn sync_accounts(&self, api: &Api<'_>) -> Result<(), ()> {
	let services = &self.services;
	let server_user: &UserId = &services.globals.server_user;
	let users: Vec<OwnedUserId> = services
		.users
		.stream()
		.ready_filter(|user| *user != server_user && services.globals.user_is_local(user))
		.ready_filter(|user| chosen(&services.server.config.twin_only_users, user))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let (mut sent, mut failed) = (0_usize, 0_usize);
	for user in &users {
		let localpart = user.localpart();
		let mut account = self.account_state(user).await;
		let digest = account.digest();
		let unchanged = self
			.pushed
			.lock()
			.expect("locked")
			.get(localpart)
			.is_some_and(|pushed| *pushed == digest);

		if unchanged {
			continue;
		}

		if !account.deactivated {
			account.welcome = services.notices.welcome_for_twin(localpart).await;
		}

		let path = ["_anoncat", "twin", "v1", "users", localpart];
		match call::<_, AccountResult>(api, Method::PUT, &path, &account).await {
			| Ok(_) | Err(CallError::Refused(_)) => {
				if account.welcome.is_some() {
					services
						.notices
						.twin_welcome_sent(localpart)
						.await
						.ok();
				}

				self.pushed
					.lock()
					.expect("locked")
					.insert(localpart.to_owned(), digest);
				sent = sent.saturating_add(1);
			},
			| Err(CallError::Unreachable) => return Err(()),
			| Err(CallError::Failed(e)) => {
				debug_warn!("Twin account not updated: {e}");
				failed = failed.saturating_add(1);
			},
		}
	}

	if sent > 0 {
		debug_info!(sent, "Twin accounts sent");
	}

	if failed > 0 {
		warn!(failed, "Twin accounts not updated; trying again next run");
	}

	Ok(())
}

/// The account as its twin should be.
#[implement(Service)]
async fn account_state(&self, user: &UserId) -> Account {
	let services = &self.services;
	let stored = services
		.users
		.password_hash(user)
		.await
		.unwrap_or_default();

	let deactivated = stored.is_empty();
	let displayname = if deactivated {
		None
	} else {
		services.profile.displayname(user).await.ok()
	};

	Account {
		password_hash: is_phc_hash(&stored).then_some(stored),
		deactivated,
		displayname,
		welcome: None,
	}
}

/// Invites and joins twins to the rooms they belong in, at most
/// `twin_room_joins_per_run` rooms a run, going on next run from where this
/// one stopped.
#[implement(Service)]
async fn sync_rooms(&self, api: &Api<'_>) -> Result<(), ()> {
	let services = &self.services;
	let config = &services.server.config;
	let Some(twin_server) = &config.twin_server_name else {
		return Ok(());
	};

	let server_user: &UserId = &services.globals.server_user;
	let mut users: Vec<OwnedUserId> = services
		.users
		.list_local_users()
		.ready_filter(|user| *user != server_user)
		.ready_filter(|user| chosen(&config.twin_only_users, user))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	users.sort_unstable_by(|a, b| a.localpart().cmp(b.localpart()));

	let start = self
		.cursor
		.lock()
		.expect("locked")
		.as_deref()
		.map_or(0, |cursor| users.partition_point(|user| user.localpart() < cursor));

	let mut budget = config.twin_room_joins_per_run;
	let (mut done, mut failed) = (0_usize, 0_usize);
	let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
	for user in users[start..].iter().chain(&users[..start]) {
		let Ok(twin) = UserId::parse_with_server_name(user.localpart(), twin_server) else {
			continue;
		};

		let rooms: Vec<OwnedRoomId> = services
			.state_cache
			.rooms_joined(user)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		let dms = self.direct_rooms(user).await;
		for room_id in &rooms {
			let membership = services
				.state_accessor
				.get_member(room_id, &twin)
				.await
				.ok()
				.map(|member| member.membership);

			// Joined is done; left, refused or banned is the twin's or the
			// room's choice, not asked again.
			let invited = match membership {
				| None => false,
				| Some(MembershipState::Invite) => true,
				| Some(_) => continue,
			};

			if !self
				.room_qualifies(user, room_id, dms.contains(room_id))
				.await
			{
				continue;
			}

			if budget == 0 {
				*self.cursor.lock().expect("locked") = Some(user.localpart().to_owned());
				log_rooms(done, failed, &reasons);
				return Ok(());
			}

			budget = budget.saturating_sub(1);
			match self
				.add_twin(api, user, &twin, room_id, invited)
				.boxed()
				.await
			{
				| Ok(()) => done = done.saturating_add(1),
				| Err(CallError::Unreachable) => {
					*self.cursor.lock().expect("locked") = Some(user.localpart().to_owned());
					log_rooms(done, failed, &reasons);
					return Err(());
				},
				| Err(CallError::Refused(e) | CallError::Failed(e)) => {
					let reason = reasons.entry(without_ids(&e)).or_default();
					*reason = reason.saturating_add(1);
					failed = failed.saturating_add(1);
				},
			}
		}
	}

	log_rooms(done, failed, &reasons);

	Ok(())
}

fn log_rooms(done: usize, failed: usize, reasons: &BTreeMap<String, usize>) {
	if done > 0 {
		info!(done, "Twins added to rooms");
	}

	if failed > 0 {
		warn!(failed, "Twins not added to some rooms; trying again next run");
		for (reason, count) in reasons.iter().take(5) {
			warn!(count, "Twin not added to a room: {reason}");
		}
	}
}

/// An error's text without the user, room and event IDs in it, so the log
/// says why without saying who or where.
fn without_ids(text: &str) -> String {
	text.split_inclusive(char::is_whitespace)
		.map(|word| match word.trim_start_matches(['"', '\'', '(', '`']).chars().next() {
			| Some('@' | '!' | '$' | '#') => "… ",
			| _ => word,
		})
		.collect::<String>()
		.trim_end()
		.chars()
		.take(300)
		.collect()
}

/// The rooms in the person's `m.direct`.
#[implement(Service)]
async fn direct_rooms(&self, user: &UserId) -> HashSet<OwnedRoomId> {
	self.services
		.account_data
		.get_global::<DirectEvent>(user, GlobalAccountDataEventType::Direct)
		.await
		.map(|direct| direct.content.0.into_values().flatten().collect())
		.unwrap_or_default()
}

#[implement(Service)]
async fn room_qualifies(&self, user: &UserId, room_id: &RoomId, is_dm: bool) -> bool {
	let services = &self.services;
	if is_dm {
		return false;
	}

	let federates = services
		.state_accessor
		.room_state_get_content::<RoomCreateEventContent>(room_id, &StateEventType::RoomCreate, "")
		.await
		.is_ok_and(|create| create.federate);

	let public = matches!(services.state_accessor.get_join_rules(room_id).await, JoinRule::Public);

	let user_is_admin = services
		.state_accessor
		.get_power_levels(room_id)
		.await
		.is_ok_and(|power_levels| is_admin(power_levels.for_user(user)));

	let notice = services
		.notices
		.is_notice_room(user, room_id)
		.await
		.unwrap_or(true);

	room_qualifies(RoomFacts { is_dm, federates, public, user_is_admin, notice })
}

/// Invites the twin (unless it is invited already), gives it the person's
/// power level when the person can, and has the twin side join it.
#[implement(Service)]
async fn add_twin(
	&self,
	api: &Api<'_>,
	user: &UserId,
	twin: &UserId,
	room_id: &RoomId,
	invited: bool,
) -> Result<(), CallError> {
	if !invited {
		self.invite_twin(user, twin, room_id)
			.await
			.map_err(|e| CallError::Failed(e.to_string()))?;
	}

	let request = JoinRequest {
		room_id: room_id.to_owned(),
		via: vec![self.services.globals.server_name().to_owned()],
	};

	let path = ["_anoncat", "twin", "v1", "users", twin.localpart(), "join"];
	call::<_, serde_json::Value>(api, Method::POST, &path, &request)
		.await
		.map(|_| ())
}

#[implement(Service)]
async fn invite_twin(&self, user: &UserId, twin: &UserId, room_id: &RoomId) -> Result {
	let services = &self.services;
	let power_levels = services
		.state_accessor
		.get_power_levels(room_id)
		.await?;

	let members: Vec<(OwnedUserId, UserPowerLevel, bool)> = services
		.state_cache
		.local_users_in_room(room_id)
		.map(|member| {
			let level = power_levels.for_user(member);
			let can_invite = power_levels.user_can_invite(member);
			(member.to_owned(), level, can_invite)
		})
		.collect()
		.await;

	let Some(inviter) = pick_inviter(
		user,
		power_levels.user_can_invite(user),
		members
			.iter()
			.map(|(member, level, can)| (member.as_ref(), *level, *can)),
	) else {
		return tuwunel_core::Err!("Nobody here can invite to the room.");
	};

	services
		.membership
		.invite(inviter, twin, room_id, None, false)
		.boxed()
		.await?;

	self.grant_level(&power_levels, user, twin, room_id)
		.await;

	Ok(())
}

/// The twin gets the person's power level, sent as the person, when it is
/// above the room's default and the person may grant it. Otherwise nothing.
#[implement(Service)]
async fn grant_level(
	&self,
	power_levels: &RoomPowerLevels,
	user: &UserId,
	twin: &UserId,
	room_id: &RoomId,
) {
	let services = &self.services;
	let user_level = power_levels.for_user(user);
	let Some(level) = twin_level(user_level, power_levels.users_default) else {
		return;
	};

	if !can_grant(
		user_level,
		level,
		power_levels.user_can_change_user_power_level(user, twin),
	) {
		return;
	}

	let state_lock = services.state.mutex.lock(room_id).await;
	let Ok(mut content) = services
		.state_accessor
		.room_state_get_content::<RoomPowerLevelsEventContent>(
			room_id,
			&StateEventType::RoomPowerLevels,
			"",
		)
		.await
	else {
		return;
	};

	if content.users.get(twin) == Some(&level) {
		return;
	}

	content.users.insert(twin.to_owned(), level);
	if let Err(e) = services
		.timeline
		.build_and_append_pdu(PduBuilder::state(String::new(), &content), user, room_id, &state_lock)
		.boxed()
		.await
	{
		debug!("Twin's power level not set: {e}");
	}
}

/// One request to the twin side, JSON both ways.
async fn call<B, R>(api: &Api<'_>, method: Method, path: &[&str], body: &B) -> Result<R, CallError>
where
	B: Serialize + Sync,
	R: DeserializeOwned,
{
	let mut url = api.base.clone();
	url.path_segments_mut()
		.map_err(|()| CallError::Refused("twin_api can't take a path".into()))?
		.clear()
		.extend(path);

	let body = serde_json::to_vec(body).map_err(|e| CallError::Failed(e.to_string()))?;
	let response = api
		.client
		.request(method, url)
		.bearer_auth(&api.secret)
		.header(CONTENT_TYPE, "application/json")
		.body(body)
		.send()
		.await
		.map_err(|e| {
			if e.is_connect() || e.is_timeout() {
				CallError::Unreachable
			} else {
				CallError::Failed(e.without_url().to_string())
			}
		})?;

	let status = response.status();
	let bytes = response
		.bytes()
		.await
		.map_err(|e| CallError::Failed(e.without_url().to_string()))?;

	if !status.is_success() {
		let error = serde_json::from_slice::<serde_json::Value>(&bytes)
			.ok()
			.and_then(|body| body.get("error")?.as_str().map(ToOwned::to_owned))
			.unwrap_or_default();

		let error = format!("{status}: {error}");
		return Err(if status.is_client_error() {
			CallError::Refused(error)
		} else {
			CallError::Failed(error)
		});
	}

	serde_json::from_slice(&bytes).map_err(|e| CallError::Failed(e.to_string()))
}

fn is_admin(level: UserPowerLevel) -> bool { level >= UserPowerLevel::Int(Int::from(ADMIN_LEVEL)) }

/// Twins go to rooms that federate and aren't direct chats or the notices
/// room, and to public rooms only when the person runs them.
fn room_qualifies(facts: RoomFacts) -> bool {
	!facts.is_dm && facts.federates && !facts.notice && (!facts.public || facts.user_is_admin)
}

/// Who invites the twin: the person when they may, else the local member
/// with the highest power level who may.
fn pick_inviter<'a>(
	user: &'a UserId,
	user_can_invite: bool,
	members: impl IntoIterator<Item = (&'a UserId, UserPowerLevel, bool)>,
) -> Option<&'a UserId> {
	if user_can_invite {
		return Some(user);
	}

	members
		.into_iter()
		.filter(|(_, _, can_invite)| *can_invite)
		.max_by_key(|(_, level, _)| *level)
		.map(|(member, ..)| member)
}

/// The level the twin gets: the person's (100 for a creator), when that is
/// above the room's default.
fn twin_level(user_level: UserPowerLevel, users_default: Int) -> Option<Int> {
	let level = match user_level {
		| UserPowerLevel::Infinite => Int::from(ADMIN_LEVEL),
		| UserPowerLevel::Int(level) => level,
	};

	(level > users_default).then_some(level)
}

/// Whether the person may give the twin that level: they may change the
/// twin's level and aren't below it.
fn can_grant(user_level: UserPowerLevel, level: Int, may_change: bool) -> bool {
	may_change && user_level >= UserPowerLevel::Int(level)
}


/// Whether the account gets a twin: everyone, unless `twin_only_users`
/// names some.
fn chosen(only: &[String], user: &UserId) -> bool {
	only.is_empty() || only.iter().any(|name| name == user.localpart())
}

#[cfg(test)]
mod tests {
	use ruma::{Int, events::room::power_levels::UserPowerLevel, user_id};

	use super::{RoomFacts, can_grant, chosen, is_admin, pick_inviter, room_qualifies, twin_level};

	#[test]
	fn reasons_keep_no_ids() {
		assert_eq!(
			super::without_ids("User @cat:example.com can't join !abc:example.com now"),
			"User … can't join … now"
		);
	}

	#[test]
	fn an_empty_list_means_everyone() {
		let user = user_id!("@cat:example.com");
		assert!(chosen(&[], user));
		assert!(chosen(&["cat".to_owned()], user));
		assert!(!chosen(&["dog".to_owned()], user));
	}

	fn private_room() -> RoomFacts { RoomFacts { federates: true, ..RoomFacts::default() } }

	#[test]
	fn private_rooms_qualify() {
		assert!(room_qualifies(private_room()));
	}

	#[test]
	fn direct_chats_never_qualify() {
		let facts = RoomFacts { is_dm: true, user_is_admin: true, ..private_room() };
		assert!(!room_qualifies(facts));
	}

	#[test]
	fn public_rooms_only_when_the_person_runs_them() {
		let public = RoomFacts { public: true, ..private_room() };
		assert!(!room_qualifies(public));
		assert!(room_qualifies(RoomFacts { user_is_admin: true, ..public }));
	}

	#[test]
	fn rooms_that_dont_federate_never_qualify() {
		let facts = RoomFacts { federates: false, user_is_admin: true, ..private_room() };
		assert!(!room_qualifies(facts));
		assert!(!room_qualifies(RoomFacts { notice: true, ..private_room() }));
	}

	#[test]
	fn admin_is_100_or_creator() {
		assert!(is_admin(UserPowerLevel::Infinite));
		assert!(is_admin(UserPowerLevel::Int(Int::from(100))));
		assert!(!is_admin(UserPowerLevel::Int(Int::from(99))));
	}

	#[test]
	fn the_person_invites_when_they_can() {
		let user = user_id!("@alice:example.com");
		let admin = user_id!("@admin:example.com");
		let members = [(admin, UserPowerLevel::Int(Int::from(100)), true)];

		assert_eq!(pick_inviter(user, true, members), Some(user));
	}

	#[test]
	fn otherwise_the_highest_member_who_can() {
		let user = user_id!("@alice:example.com");
		let moderator = user_id!("@mod:example.com");
		let admin = user_id!("@admin:example.com");
		let muted = user_id!("@muted:example.com");
		let members = [
			(moderator, UserPowerLevel::Int(Int::from(50)), true),
			(admin, UserPowerLevel::Int(Int::from(100)), true),
			(muted, UserPowerLevel::Infinite, false),
		];

		assert_eq!(pick_inviter(user, false, members), Some(admin));
		assert_eq!(pick_inviter(user, false, [(muted, UserPowerLevel::Infinite, false)]), None);
	}

	#[test]
	fn twin_gets_the_persons_level_above_the_default() {
		let default = Int::from(0);
		assert_eq!(twin_level(UserPowerLevel::Int(Int::from(50)), default), Some(Int::from(50)));
		assert_eq!(twin_level(UserPowerLevel::Infinite, default), Some(Int::from(100)));
		assert_eq!(twin_level(UserPowerLevel::Int(default), default), None);
	}

	#[test]
	fn granting_needs_the_right_and_the_level() {
		let fifty = Int::from(50);
		assert!(can_grant(UserPowerLevel::Int(fifty), fifty, true));
		assert!(can_grant(UserPowerLevel::Infinite, Int::from(100), true));
		assert!(!can_grant(UserPowerLevel::Int(fifty), fifty, false));
		assert!(!can_grant(UserPowerLevel::Int(fifty), Int::from(100), true));
	}
}
