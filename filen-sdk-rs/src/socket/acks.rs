//! socket.io acks: the `43<id>[<args...>]` frames that answer an event sent as
//! `42<id>[<event>, <args...>]`, and the acks one connection still waits for.

use std::{
	collections::{HashMap, hash_map::Entry},
	mem,
};

use serde::de::DeserializeOwned;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use super::consts::MESSAGE_ACK_PREFIX;

/// An ack frame: the id of the event it answers and its arguments, a JSON array not yet parsed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Ack<'a> {
	id: u32,
	args: &'a str,
}

impl Ack<'_> {
	pub(super) fn id(&self) -> u32 {
		self.id
	}

	/// The ack's arguments when there is exactly one, as `T`.
	pub(super) fn single_arg<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
		single_arg(self.args)
	}

	/// The ack's arguments, kept for whoever waits for them to parse.
	pub(super) fn to_owned_args(&self) -> AckArgs {
		AckArgs(self.args.to_owned())
	}
}

/// An ack's arguments, a JSON array not yet parsed, handed to the task that waits for the ack so
/// the connection's loop does no parsing for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AckArgs(String);

impl AckArgs {
	/// The arguments when there is exactly one, as `T`.
	pub(crate) fn single_arg<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
		single_arg(&self.0)
	}
}

fn single_arg<T: DeserializeOwned>(args: &str) -> Result<T, serde_json::Error> {
	serde_json::from_str::<(T,)>(args).map(|(arg,)| arg)
}

/// Why a frame that starts like an ack is not one this client can have asked for.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AckParseError {
	#[error("ack frame without an id")]
	MissingId,
	/// The server echoes the id as a number, so `<id>` never has a leading zero.
	#[error("ack id with a leading zero")]
	LeadingZero,
	/// Ids are `u32`, so a larger one answers no event of ours.
	#[error("ack id out of range")]
	IdOutOfRange,
	#[error("ack frame without an argument array")]
	MissingArgs,
}

/// Parses `frame` as an ack: `None` when it is not one (it does not start with `43`).
pub(super) fn parse_ack(frame: &str) -> Option<Result<Ack<'_>, AckParseError>> {
	let rest = frame.strip_prefix(MESSAGE_ACK_PREFIX)?;
	let digits = rest
		.bytes()
		.position(|b| !b.is_ascii_digit())
		.unwrap_or(rest.len());
	// `digits` counts ASCII bytes from the start, so it is a char boundary.
	let Some((id, args)) = rest.split_at_checked(digits) else {
		return Some(Err(AckParseError::MissingArgs));
	};
	Some(parse_id(id).and_then(|id| {
		if args.starts_with('[') {
			Ok(Ack { id, args })
		} else {
			Err(AckParseError::MissingArgs)
		}
	}))
}

fn parse_id(digits: &str) -> Result<u32, AckParseError> {
	match digits.as_bytes() {
		[] => Err(AckParseError::MissingId),
		[b'0', _, ..] => Err(AckParseError::LeadingZero),
		// Only ASCII digits: the parse can fail only on overflow.
		_ => digits.parse().map_err(|_| AckParseError::IdOutOfRange),
	}
}

/// The acks one connection waits for, by id, each with whoever waits for it.
///
/// Ids count up from 0 for each connection and skip the ones still pending. An ack whose deadline
/// passed is abandoned: its waiter was given up on, and its id stays taken until the ack arrives
/// (it is then stale) or the connection ends, since the server still counts the event as
/// unanswered.
pub(super) struct PendingAcks<W> {
	next_id: u32,
	entries: HashMap<u32, Pending<W>>,
}

enum Pending<W> {
	Waiting { deadline: Instant, waiter: W },
	Abandoned,
}

/// What an arriving ack settles.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Settled<W> {
	/// The ack answers a pending event: here is its waiter.
	Answered(W),
	/// The ack answers an event whose deadline passed; nobody waits for it any more.
	Stale,
	/// No event of this connection has that id pending.
	Unknown,
}

impl<W> Default for PendingAcks<W> {
	fn default() -> Self {
		Self {
			next_id: 0,
			entries: HashMap::new(),
		}
	}
}

impl<W> PendingAcks<W> {
	/// Registers `waiter` for an ack due by `deadline` and returns the id to send the event
	/// with, or hands `waiter` back when every id is taken.
	pub(super) fn insert(&mut self, waiter: W, deadline: Instant) -> Result<u32, W> {
		// Of `len + 1` consecutive ids at least one is free, unless every id is taken.
		for _ in 0..=self.entries.len() {
			let id = self.next_id;
			self.next_id = self.next_id.wrapping_add(1);
			if let Entry::Vacant(entry) = self.entries.entry(id) {
				entry.insert(Pending::Waiting { deadline, waiter });
				return Ok(id);
			}
		}
		Err(waiter)
	}

	/// Settles the ack with `id`, freeing the id.
	pub(super) fn settle(&mut self, id: u32) -> Settled<W> {
		match self.entries.remove(&id) {
			Some(Pending::Waiting { waiter, .. }) => Settled::Answered(waiter),
			Some(Pending::Abandoned) => Settled::Stale,
			None => Settled::Unknown,
		}
	}

	/// How many ids are taken: the acks waited for, and those given up on that did not arrive.
	pub(super) fn len(&self) -> usize {
		self.entries.len()
	}

	/// The waiters of every ack still waited for, which no ack will reach any more: the
	/// connection ended.
	pub(super) fn into_waiters(self) -> impl Iterator<Item = W> {
		self.entries
			.into_values()
			.filter_map(|pending| match pending {
				Pending::Waiting { waiter, .. } => Some(waiter),
				Pending::Abandoned => None,
			})
	}

	/// The earliest deadline of an ack still waited for.
	pub(super) fn next_deadline(&self) -> Option<Instant> {
		self.entries
			.values()
			.filter_map(|pending| match pending {
				Pending::Waiting { deadline, .. } => Some(*deadline),
				Pending::Abandoned => None,
			})
			.min()
	}

	/// Abandons every ack due by `now` and returns their waiters.
	pub(super) fn expire(&mut self, now: Instant) -> Vec<W> {
		let mut expired = Vec::new();
		for pending in self.entries.values_mut() {
			if matches!(pending, Pending::Waiting { deadline, .. } if *deadline <= now)
				&& let Pending::Waiting { waiter, .. } = mem::replace(pending, Pending::Abandoned)
			{
				expired.push(waiter);
			}
		}
		expired
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use super::AckArgs;

	/// The arguments `json` (a JSON array) of an ack a test plays the server for.
	pub(crate) fn ack_args(json: &str) -> AckArgs {
		AckArgs(json.to_owned())
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	/// What [`parse_ack`] makes of a frame, with the ack's fields laid out.
	type ParsedAck<'a> = Option<Result<(u32, &'a str), AckParseError>>;

	#[test]
	fn ack_frames_parse_into_their_id_and_arguments() {
		let cases: [(&str, ParsedAck<'_>); 14] = [
			(
				r#"430[{"version":2,"ttlSeconds":60}]"#,
				Some(Ok((0, r#"[{"version":2,"ttlSeconds":60}]"#))),
			),
			(
				r#"4312[{"status":true}]"#,
				Some(Ok((12, r#"[{"status":true}]"#))),
			),
			("434294967295[]", Some(Ok((u32::MAX, "[]")))),
			("434294967296[]", Some(Err(AckParseError::IdOutOfRange))),
			(
				"4399999999999999999999[]",
				Some(Err(AckParseError::IdOutOfRange)),
			),
			("4307[]", Some(Err(AckParseError::LeadingZero))),
			("4300[]", Some(Err(AckParseError::LeadingZero))),
			("43[]", Some(Err(AckParseError::MissingId))),
			("43", Some(Err(AckParseError::MissingId))),
			("437", Some(Err(AckParseError::MissingArgs))),
			("437{}", Some(Err(AckParseError::MissingArgs))),
			(r#"42["authed",true]"#, None),
			(r#"421["lockHello"]"#, None),
			("3", None),
		];
		for (frame, expected) in cases {
			let parsed = parse_ack(frame).map(|ack| ack.map(|ack| (ack.id, ack.args)));
			assert_eq!(parsed, expected, "{frame}");
		}
	}

	#[test]
	fn an_ack_argument_is_read_only_when_it_is_the_only_one() {
		let ack = |frame| parse_ack(frame).unwrap().unwrap();
		assert_eq!(ack("431[7]").single_arg::<u32>().unwrap(), 7);
		assert!(ack("431[7,8]").single_arg::<u32>().is_err());
		assert!(ack("431[]").single_arg::<u32>().is_err());
		assert!(ack(r#"431["x"]"#).single_arg::<u32>().is_err());
	}

	#[tokio::test(start_paused = true)]
	async fn ids_count_up_from_zero_and_skip_pending_ones() {
		let deadline = Instant::now() + Duration::from_secs(5);
		let mut pending = PendingAcks::default();
		assert_eq!(pending.insert('a', deadline), Ok(0));
		assert_eq!(pending.insert('b', deadline), Ok(1));
		assert_eq!(pending.settle(0), Settled::Answered('a'));

		pending.next_id = u32::MAX;
		assert_eq!(pending.insert('c', deadline), Ok(u32::MAX));
		// 0 is free again, 1 is still pending.
		assert_eq!(pending.insert('d', deadline), Ok(0));
		assert_eq!(pending.insert('e', deadline), Ok(2));
		assert_eq!(pending.settle(1), Settled::Answered('b'));
		assert_eq!(pending.settle(u32::MAX), Settled::Answered('c'));
	}

	#[tokio::test(start_paused = true)]
	async fn an_ack_after_its_deadline_is_stale_and_frees_its_id() {
		let begin = Instant::now();
		let mut pending = PendingAcks::default();
		assert_eq!(pending.insert('a', begin + Duration::from_secs(5)), Ok(0));
		assert_eq!(pending.insert('b', begin + Duration::from_secs(10)), Ok(1));
		assert_eq!(
			pending.next_deadline(),
			Some(begin + Duration::from_secs(5))
		);

		assert_eq!(
			pending.expire(begin + Duration::from_secs(4)),
			Vec::<char>::new()
		);
		assert_eq!(pending.expire(begin + Duration::from_secs(5)), ['a']);
		assert_eq!(
			pending.expire(begin + Duration::from_secs(6)),
			Vec::<char>::new()
		);
		assert_eq!(
			pending.next_deadline(),
			Some(begin + Duration::from_secs(10))
		);

		// The abandoned id stays taken until its ack arrives.
		pending.next_id = 0;
		assert_eq!(pending.insert('c', begin), Ok(2));
		assert_eq!(pending.settle(0), Settled::Stale);
		assert_eq!(pending.settle(0), Settled::Unknown);
		assert_eq!(pending.settle(7), Settled::Unknown);
	}
}
