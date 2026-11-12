use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt,
	UserId,
	events::{
		TimelineEventType,
		room::{
			member::{MembershipState, RoomMemberEventContent},
			redaction::RoomRedactionEventContent,
		},
	},
};
use serde_json::Value as JsonValue;
use synapse_admin_api::users::redact::v1::{Request, Response};
use tuwunel_core::{
	Err, Result,
	utils::{
		BoolExt,
		stream::TryReadyExt,
	},
};
use tuwunel_matrix::{
	Event,
	pdu::{PduBuilder, PduEvent},
};

use crate::{Ruma, require_admin};

/// Synapse defaults an omitted or zero limit to 1000 events per room.
const LIMIT_DEFAULT: usize = 1000;

type FailedRedactions = BTreeMap<OwnedEventId, String>;

/// Kinds of event removed along with messages: what a person says or posts.
const SAID: &[&str] = &[
	"m.room.message",
	"m.room.encrypted",
	"m.sticker",
	"m.reaction",
	"m.poll.start",
	"org.matrix.msc3381.poll.start",
];

struct RedactArgs {
	user_id: OwnedUserId,
	rooms: Vec<OwnedRoomId>,
	redact_as: OwnedUserId,
	/// Tried when `redact_as` can't redact in a room, such as a local user
	/// who already left it.
	admin: OwnedUserId,
	reason: Option<String>,
	limit: usize,
	before_ts: Option<MilliSecondsSinceUnixEpoch>,
	after_ts: Option<MilliSecondsSinceUnixEpoch>,
}

/// # `POST /_synapse/admin/v1/user/{user_id}/redact`
///
/// Schedules a background task redacting the user's messages, encrypted events,
/// and join events in the given rooms (all joined and banned rooms when empty),
/// returning the task id for the redact-status endpoint.
pub(crate) async fn admin_redact_user_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;

	let in_progress = services
		.tasks
		.has_nonterminal(super::REDACT_USER_ACTION, user_id.as_str());

	if in_progress {
		return Err!(Request(InvalidParam("Redact already in progress for user {user_id}")));
	}

	let rooms = body
		.rooms
		.is_empty()
		.then_async(async || rooms_ever_in(&services, user_id).await)
		.await;

	let redact_as = (body.use_admin || !services.globals.user_is_local(user_id))
		.then(|| body.sender_user().to_owned());

	let admin = body.sender_user().to_owned();
	let Request {
		user_id,
		rooms: requested_rooms,
		reason,
		limit,
		before_ts,
		after_ts,
		..
	} = body.body;

	let rooms = rooms.unwrap_or(requested_rooms);
	let resource_id = user_id.to_string();
	let redact_as = redact_as.unwrap_or_else(|| user_id.clone());

	let args = RedactArgs {
		user_id,
		rooms,
		redact_as,
		admin,
		reason,
		limit: resolve_limit(limit),
		before_ts,
		after_ts,
	};

	let redact_id = services
		.tasks
		.spawn(super::REDACT_USER_ACTION, resource_id, redact_events(services, args))
		.to_string();

	Ok(Response { redact_id })
}

/// Every room the user is in or has left, kicked and banned included: what
/// they said stays in a room after they leave it.
async fn rooms_ever_in(services: &crate::State, user_id: &UserId) -> Vec<OwnedRoomId> {
	let joined = services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned);

	let left = services
		.state_cache
		.rooms_left(user_id)
		.map(ToOwned::to_owned);

	joined.chain(left).collect().await
}

fn resolve_limit(limit: Option<UInt>) -> usize {
	limit
		.and_then(|limit| limit.try_into().ok())
		.filter(|&limit| limit > 0)
		.unwrap_or(LIMIT_DEFAULT)
}

// The detached task needs its own static services handle.
/// Redacts the events, and swaps any media they posted for the stand-in: the
/// file is deleted here, and anyone asking for it later gets the stand-in.
/// That covers files from other servers too, which aren't recorded as anyone's
/// uploads here.
async fn redact_events(services: crate::State, args: RedactArgs) -> Result<JsonValue> {
	let mut failed = FailedRedactions::new();
	let mut media = BTreeSet::new();

	for room_id in &args.rooms {
		// A room deleted here has nothing left to remove; one bad room
		// mustn't stop the rest.
		let Ok(pdus) = services
			.timeline
			.pdus_rev(None, room_id, None)
			.ready_try_filter(|(_, pdu)| is_candidate(pdu, &args))
			.take(args.limit)
			.ready_try_filter(|(_, pdu)| is_eligible(pdu))
			.map_ok(|(_, pdu)| pdu)
			.try_collect::<Vec<PduEvent>>()
			.await
		else {
			continue;
		};

		for pdu in pdus {
			if let Ok(content) = pdu.get_content::<JsonValue>() {
				media_links(&content, &mut media);
			}

			if let Err(e) = redact_one(&services, room_id, &pdu.event_id, &args).await {
				failed.insert(pdu.event_id, e.to_string());
			}
		}
	}

	let note = format!("posted by {}", args.user_id);
	for mxc in &media {
		services.media.swap(mxc, &note).await.ok();
	}

	Ok(task_result(&failed))
}

/// Every `mxc://` link in an event's content: the file, its thumbnail, and
/// links nested anywhere else.
fn media_links(value: &JsonValue, out: &mut BTreeSet<String>) {
	match value {
		| JsonValue::String(s) if s.starts_with("mxc://") => {
			out.insert(s.clone());
		},
		| JsonValue::Array(items) => items.iter().for_each(|v| media_links(v, out)),
		| JsonValue::Object(map) => map.values().for_each(|v| media_links(v, out)),
		| _ => {},
	}
}

fn is_candidate(pdu: &PduEvent, args: &RedactArgs) -> bool {
	pdu.sender == args.user_id
		&& args
			.before_ts
			.is_none_or(|ts| pdu.origin_server_ts() <= ts)
		&& args
			.after_ts
			.is_none_or(|ts| pdu.origin_server_ts() >= ts)
		&& (pdu.kind == TimelineEventType::RoomMember
			|| SAID.contains(&pdu.kind.to_string().as_str()))
}

fn is_eligible(pdu: &PduEvent) -> bool {
	!pdu.is_redacted()
		&& (pdu.kind != TimelineEventType::RoomMember
			|| pdu
				.get_content()
				.is_ok_and(|member: RoomMemberEventContent| {
					member.membership == MembershipState::Join
				}))
}

async fn redact_one(
	services: &crate::State,
	room_id: &RoomId,
	event_id: &EventId,
	args: &RedactArgs,
) -> Result<()> {
	// Their own account first, so it reads as theirs; once they've left the
	// room that can't work, and the admin does it if they're a moderator there.
	let mut redact_as = None;
	for who in [&args.redact_as, &args.admin] {
		if services
			.state_cache
			.is_joined(who, room_id)
			.await
			&& services
				.state_accessor
				.user_can_redact(event_id, who, room_id, false)
				.await
				.unwrap_or(false)
		{
			redact_as = Some(who);
			break;
		}
	}

	let Some(redact_as) = redact_as else {
		return Err!(Request(Forbidden(
			"Neither they nor you can remove messages in this room; you'd need to be a moderator \
			 there."
		)));
	};

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				redacts: Some(event_id.to_owned()),
				..PduBuilder::timeline(&RoomRedactionEventContent {
					redacts: Some(event_id.to_owned()),
					reason: args.reason.clone(),
				})
			},
			redact_as,
			room_id,
			&state_lock,
		)
		.await
		.map(|_| ())
}

fn task_result(failed_redactions: &FailedRedactions) -> JsonValue {
	serde_json::json!({ "failed_redactions": failed_redactions })
}

#[cfg(test)]
mod tests {
	use ruma::{MilliSecondsSinceUnixEpoch, UInt, event_id, uint, user_id};
	use serde_json::json;

	use super::{
		FailedRedactions, PduEvent, RedactArgs, is_candidate, is_eligible, resolve_limit,
		task_result,
	};

	fn args(before_ts: Option<UInt>, after_ts: Option<UInt>) -> RedactArgs {
		RedactArgs {
			user_id: user_id!("@alice:example.com").to_owned(),
			rooms: Vec::new(),
			redact_as: user_id!("@alice:example.com").to_owned(),
			admin: user_id!("@admin:example.com").to_owned(),
			reason: None,
			limit: super::LIMIT_DEFAULT,
			before_ts: before_ts.map(MilliSecondsSinceUnixEpoch),
			after_ts: after_ts.map(MilliSecondsSinceUnixEpoch),
		}
	}

	fn pdu(kind: &str, sender: &str, ts: u64, content: &serde_json::Value) -> PduEvent {
		pdu_with_unsigned(kind, sender, ts, content, &json!({}))
	}

	fn pdu_with_unsigned(
		kind: &str,
		sender: &str,
		ts: u64,
		content: &serde_json::Value,
		unsigned: &serde_json::Value,
	) -> PduEvent {
		serde_json::from_value(json!({
			"type": kind,
			"content": content,
			"event_id": "$e:example.com",
			"room_id": "!room:example.com",
			"sender": sender,
			"prev_events": ["$prev:example.com"],
			"auth_events": ["$auth:example.com"],
			"origin_server_ts": ts,
			"depth": 12,
			"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
			"unsigned": unsigned,
		}))
		.expect("valid pdu")
	}

	fn redacted_message() -> PduEvent {
		pdu_with_unsigned(
			"m.room.message",
			"@alice:example.com",
			1000,
			&json!({}),
			&json!({ "redacted_because": {} }),
		)
	}

	#[test]
	fn resolve_limit_defaults_zero_and_absent_to_1000() {
		assert_eq!(resolve_limit(None), super::LIMIT_DEFAULT);
		assert_eq!(resolve_limit(Some(uint!(0))), super::LIMIT_DEFAULT);
		assert_eq!(resolve_limit(Some(uint!(25))), 25);
	}

	#[test]
	fn task_result_always_carries_the_failed_redactions_key() {
		assert_eq!(task_result(&FailedRedactions::new()), json!({ "failed_redactions": {} }));

		let one: FailedRedactions =
			[(event_id!("$f:example.com").to_owned(), "boom".to_owned())].into();

		let value = task_result(&one);

		assert_eq!(value, json!({ "failed_redactions": { "$f:example.com": "boom" } }));

		let parsed: FailedRedactions = serde_json::from_value(value["failed_redactions"].clone())
			.expect("failed_redactions round-trips");

		assert_eq!(parsed.len(), 1);
		assert!(parsed.contains_key(event_id!("$f:example.com")));
	}

	#[test]
	fn candidate_filters_sender_type_and_window() {
		let args = args(Some(uint!(1500)), Some(uint!(500)));

		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1000, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@mallory:example.com", 1000, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.topic", "@alice:example.com", 1000, &json!({})),
			&args
		));

		// The window is inclusive at both bounds.
		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1500, &json!({})),
			&args
		));
		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 500, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1501, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@alice:example.com", 499, &json!({})),
			&args
		));
	}

	#[test]
	fn stickers_and_reactions_go_with_messages() {
		let args = args(None, None);
		for kind in ["m.sticker", "m.reaction", "m.poll.start", "m.room.encrypted"] {
			assert!(is_candidate(&pdu(kind, "@alice:example.com", 1000, &json!({})), &args), "{kind}");
		}
		assert!(!is_candidate(&pdu("m.room.redaction", "@alice:example.com", 1000, &json!({})), &args));
	}

	#[test]
	fn media_links_are_found_anywhere_in_content() {
		let mut found = std::collections::BTreeSet::new();
		super::media_links(
			&json!({
				"url": "mxc://a/1",
				"info": { "thumbnail_url": "mxc://a/2" },
				"file": { "url": "mxc://b/3" },
				"body": "not a link mxc://x/y",
				"list": ["mxc://c/4", 5],
			}),
			&mut found,
		);
		assert_eq!(found.into_iter().collect::<Vec<_>>(), ["mxc://a/1", "mxc://a/2", "mxc://b/3", "mxc://c/4"]);
	}

	#[test]
	fn eligible_keeps_joins_and_skips_redacted() {
		let join =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "join" }));

		let leave =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "leave" }));

		let invite =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "invite" }));

		let message = pdu("m.room.message", "@alice:example.com", 1000, &json!({}));

		assert!(is_eligible(&join));
		assert!(!is_eligible(&leave));
		assert!(!is_eligible(&invite));
		assert!(is_eligible(&message));
		assert!(!is_eligible(&redacted_message()));
	}
}
