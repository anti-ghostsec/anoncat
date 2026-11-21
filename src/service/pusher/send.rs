use futures::{FutureExt, future::join};
use ruma::{
	UInt, UserId,
	api::{
		client::push::{Pusher, PusherKind},
		push_gateway::send_event_notification::v1::{
			Device, Notification, NotificationCounts, Request,
		},
	},
	push::{Action, HttpPusherData, PushFormat, Ruleset},
};
use serde_json::Value;
use tuwunel_core::{Result, err, error, implement, trace, utils::BoolExt};
use tuwunel_matrix::Event;
use url::Url;

use super::Evaluate;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_push_notice<E>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	ruleset: &Ruleset,
	event: &E,
) -> Result
where
	E: Event,
{
	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(event.room_id())
		.map(Result::ok);

	let (power_levels, related_events) = join(power_levels, self.related_events(event)).await;

	let serialized = event.to_format();
	let actions = self
		.get_actions(Evaluate {
			user: user_id,
			ruleset,
			power_levels: power_levels.as_ref(),
			pdu: &serialized,
			room_id: event.room_id(),
			related_events: related_events.as_ref(),
		})
		.await;

	let notify = actions.iter().any(Action::should_notify);
	let tweak_count = actions
		.iter()
		.filter(|action| matches!(action, Action::SetTweak(_)))
		.count();

	trace!(
		%user_id,
		event_id = %event.event_id(),
		actions = %actions.len(),
		notify,
		tweaks = tweak_count,
		"Push notice decision",
	);

	if notify || self.services.config.push_everything {
		self.send_notice(user_id, pusher, event).await?;
	}

	Ok(())
}

/// Send an account-wide counts-only notification to a push gateway.
///
/// Enabled HTTP pushers emit the request, including an explicit zero. The
/// delivery is skipped only when the gateway is known to hold the current
/// total already; an unknown gateway is always sent to.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_badge_notice(&self, user_id: &UserId, pusher: &Pusher) -> Result {
	let PusherKind::Http(http) = &pusher.kind else {
		return Ok(());
	};

	if badge_count_disabled(http) {
		return Ok(());
	}

	let unread = UInt::new(self.global_notification_count(user_id).await).unwrap_or(UInt::MAX);

	if self.sent_badge(user_id, &pusher.ids.pushkey) == Some(unread) {
		return Ok(());
	}

	let device = self.prepare_http_pusher(pusher, http)?;
	let mut notify = Notification::new(vec![device]);
	notify.counts = NotificationCounts::new_explicit(Some(unread), None);

	self.send_http_notice(user_id, pusher, http, notify, Some(unread))
		.await
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_notice<Pdu: Event>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	event: &Pdu,
) -> Result {
	// TODO: email
	match &pusher.kind {
		| PusherKind::Http(http) =>
			self.send_http_event_notice(user_id, pusher, http, event)
				.await,
		// TODO: Handle email
		//PusherKind::Email(_) => Ok(()),
		| _ => Ok(()),
	}
}

/// Sends the notice for one event.
///
/// anoncat always sends the `event_id_only` format, whatever the client asked
/// for: only the event and room IDs and the unread count leave the server. The
/// push gateway, and the phone vendor's push service behind it, never see the
/// message, its sender, or the room's name; the app fetches the event itself.
#[implement(super::Service)]
async fn send_http_event_notice<Pdu: Event>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	http: &HttpPusherData,
	event: &Pdu,
) -> Result {
	let mut device = self.prepare_http_pusher(pusher, http)?;
	device.data.format = Some(PushFormat::EventIdOnly);

	let mut notify = Notification::new(vec![device]);
	notify.event_id = Some(event.event_id().to_owned());
	notify.room_id = Some(event.room_id().to_owned());

	let unread = badge_count_disabled(http)
		.is_false()
		.then_async(async || {
			UInt::new(self.global_notification_count(user_id).await).unwrap_or(UInt::MAX)
		})
		.await;

	if let Some(unread) = unread {
		notify.counts = NotificationCounts::new_explicit(Some(unread), None);
	}

	self.send_http_notice(user_id, pusher, http, notify, unread)
		.await
}

#[implement(super::Service)]
fn prepare_http_pusher(&self, pusher: &Pusher, http: &HttpPusherData) -> Result<Device> {
	let address = &http.url;
	let url = Url::parse(address).map_err(|e| {
		err!(Request(InvalidParam(
			warn!(url = %address, error = %e, "HTTP pusher URL is not a valid URL")
		)))
	})?;

	self.check_http_pusher_url(&url)?;

	let mut device = Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
	device.data.data.clone_from(&http.data);
	device.data.format.clone_from(&http.format);

	Ok(device)
}

/// Deliver one notification to the pusher's gateway and honor its verdict.
///
/// A pushkey the gateway names in `rejected` has its pusher removed. `unread`
/// names the counts value on the wire; it is recorded as delivered only after
/// the gateway accepts, so a failed or rejected send leaves the next refresh
/// unconditional.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_http_notice(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	http: &HttpPusherData,
	notify: Notification,
	unread: Option<UInt>,
) -> Result {
	let response = self
		.send_request(&http.url, Request::new(notify))
		.await?;

	let pushkey = &pusher.ids.pushkey;

	if response.rejected.contains(pushkey) {
		error!(
			url = %http.url,
			%pushkey,
			"Push gateway rejected the pushkey; removing the pusher. Push notifications \
			 for this device stop until the client registers a new pusher.",
		);

		self.delete_pusher(user_id, pushkey).await;

		return Ok(());
	}

	if let Some(unread) = unread {
		self.record_sent_badge(user_id, pushkey, unread);
	}

	Ok(())
}

fn badge_count_disabled(http: &HttpPusherData) -> bool {
	["org.matrix.msc4076.disable_badge_count", "disable_badge_count"]
		.iter()
		.any(|key| http.data.get(*key).and_then(Value::as_bool) == Some(true))
}
