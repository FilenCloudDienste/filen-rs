//! The one thread that hands a sync engine's events and watch statuses to the binding's listener.
//!
//! A pass calls its observer synchronously, under the account-wide drive-write lock, so the
//! observer only queues; this thread is where the listener runs. It drains everything queued since
//! its last round, drops each transfer's superseded `Progress` ticks, and calls the listener with
//! chunks of at most [`DELIVERY_CHUNK`] in the order they were queued. Every binding shares it: the
//! listener is a plain closure, which each binding builds around its own callback type.
//!
//! A binding guards each of its foreign calls itself, so a JS callback that throws loses only the
//! deliveries that call carried. A listener that panics all the same loses only its own chunk: an
//! unwind out of the thread would end it, and every later delivery would be dropped unseen.

use std::{
	collections::HashSet,
	mem,
	panic::{AssertUnwindSafe, catch_unwind},
	sync::{Arc, Mutex, PoisonError},
};

use tokio::sync::mpsc::{self, UnboundedSender};

use super::types::{SyncEventMessage, SyncPassEvent};
use crate::sync_engine::{TransferDirection, WatchStatus};

/// The most deliveries one listener call carries.
pub(super) const DELIVERY_CHUNK: usize = 512;

/// What the listener is handed, in the order it happened.
// Nearly every delivery is an event, a status comes once per pass: boxing the event would allocate
// once per event to shrink the rare status.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Delivery {
	Event(SyncPassEvent),
	/// A watch's status changed (and once when it starts).
	Status {
		pair_id: u64,
		status: WatchStatus,
	},
}

/// Receives one chunk of deliveries at a time, on the delivery thread.
pub(super) type Listener = Arc<dyn Fn(Vec<Delivery>) + Send + Sync>;

/// The listener an engine delivers to: replaced when another JS context attaches, read once per
/// drain. `Arc`: the engine's core and its delivery thread both hold it.
pub(super) type ListenerSlot = Arc<Mutex<Listener>>;

/// One call a binding's listener makes for a chunk: the two kinds keep their order.
#[derive(Debug, PartialEq)]
pub(super) enum ListenerCall {
	/// A run of consecutive pass events.
	Events(Vec<SyncPassEvent>),
	/// A watch's status changed (and once when it starts).
	Status { pair_id: u64, status: WatchStatus },
}

/// The calls `deliveries` make, in order: each run of consecutive events is one
/// [`ListenerCall::Events`], each status one [`ListenerCall::Status`].
pub(super) fn listener_calls(deliveries: Vec<Delivery>) -> Vec<ListenerCall> {
	let mut calls = Vec::new();
	let mut events = Vec::new();
	for delivery in deliveries {
		match delivery {
			Delivery::Event(event) => events.push(event),
			Delivery::Status { pair_id, status } => {
				if !events.is_empty() {
					calls.push(ListenerCall::Events(mem::take(&mut events)));
				}
				calls.push(ListenerCall::Status { pair_id, status });
			}
		}
	}
	if !events.is_empty() {
		calls.push(ListenerCall::Events(events));
	}
	calls
}

/// Start the delivery thread around `listener`. It ends once every returned sender (and every clone
/// of it) is gone, after delivering what they sent.
pub(super) fn spawn_delivery(listener: ListenerSlot) -> UnboundedSender<Delivery> {
	let (sender, mut receiver) = mpsc::unbounded_channel::<Delivery>();
	// A plain thread rather than a runtime task, so `blocking_recv` is valid and a slow listener
	// holds up no runtime thread.
	crate::runtime::spawn(move || {
		while let Some(first) = receiver.blocking_recv() {
			let mut drained = vec![first];
			while let Ok(next) = receiver.try_recv() {
				drained.push(next);
			}
			// Read per drain, so an attach takes effect from the next one; cloned out so the
			// listener runs without the lock, and an attach during a slow call does not wait for it.
			let current = Arc::clone(&*listener.lock().unwrap_or_else(PoisonError::into_inner));
			deliver(&*current, drained);
		}
	});
	sender
}

/// Hand one drain to `listener`, coalesced, in chunks of at most [`DELIVERY_CHUNK`]. A chunk whose
/// call panics past the binding's own guards is logged and dropped, and the rest still go.
fn deliver(listener: &(dyn Fn(Vec<Delivery>) + Send + Sync), drained: Vec<Delivery>) {
	let mut deliveries = coalesce(drained).into_iter();
	loop {
		let chunk: Vec<Delivery> = deliveries.by_ref().take(DELIVERY_CHUNK).collect();
		if chunk.is_empty() {
			return;
		}
		let len = chunk.len();
		if catch_unwind(AssertUnwindSafe(|| listener(chunk))).is_err() {
			tracing::error!("the sync engine listener panicked; {len} deliveries were lost");
		}
	}
}

/// `deliveries` without each `Progress` a later one in the same list supersedes: the same pair,
/// pass, path and direction. The latest keeps its own position, so it still comes before the event
/// that ends its transfer. Nothing else is dropped.
fn coalesce(deliveries: Vec<Delivery>) -> Vec<Delivery> {
	let mut seen = HashSet::new();
	// From the back, so the first sighting of a key is its latest tick.
	let mut keep: Vec<bool> = deliveries
		.iter()
		.rev()
		.map(|delivery| match delivery {
			Delivery::Event(SyncPassEvent {
				pair_id,
				pass_id,
				event: SyncEventMessage::Progress {
					rel_path,
					direction,
					..
				},
				..
			}) => seen.insert((
				*pair_id,
				*pass_id,
				rel_path.as_str(),
				*direction == TransferDirection::Upload,
			)),
			_ => true,
		})
		.collect();
	keep.reverse();
	deliveries
		.into_iter()
		.zip(keep)
		.filter_map(|(delivery, keep)| keep.then_some(delivery))
		.collect()
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc;

	use super::*;
	use crate::sync_engine::WatchState;

	fn event(pass_id: u64, event: SyncEventMessage) -> Delivery {
		Delivery::Event(SyncPassEvent {
			pair_id: 1,
			pass_id,
			emitted_at_ms: 0,
			event,
		})
	}

	fn progress(rel_path: &str, direction: TransferDirection, bytes: u64) -> Delivery {
		event(
			7,
			SyncEventMessage::Progress {
				rel_path: rel_path.to_owned(),
				direction,
				bytes,
				total: 100,
			},
		)
	}

	fn uploading(rel_path: &str) -> Delivery {
		event(
			7,
			SyncEventMessage::Uploading {
				rel_path: rel_path.to_owned(),
			},
		)
	}

	fn status(pair_id: u64) -> Delivery {
		Delivery::Status {
			pair_id,
			status: WatchStatus {
				state: WatchState::Running,
				..WatchStatus::default()
			},
		}
	}

	/// A listener that forwards every chunk to the test.
	fn recording() -> (ListenerSlot, mpsc::Receiver<Vec<Delivery>>) {
		let (chunks_tx, chunks) = mpsc::channel();
		let listener: Listener = Arc::new(move |chunk| {
			// the test may already have what it waits for
			let _ = chunks_tx.send(chunk);
		});
		(Arc::new(Mutex::new(listener)), chunks)
	}

	#[test]
	fn a_superseded_progress_is_dropped_and_its_successor_stays_before_the_upload() {
		let coalesced = coalesce(vec![
			progress("a", TransferDirection::Upload, 10),
			progress("b", TransferDirection::Upload, 10),
			status(1),
			progress("a", TransferDirection::Upload, 50),
			progress("a", TransferDirection::Download, 5),
			uploading("a"),
			progress("b", TransferDirection::Upload, 90),
		]);

		assert_eq!(
			coalesced,
			vec![
				status(1),
				progress("a", TransferDirection::Upload, 50),
				progress("a", TransferDirection::Download, 5),
				uploading("a"),
				progress("b", TransferDirection::Upload, 90),
			]
		);
	}

	#[test]
	fn progress_of_another_pass_or_pair_is_kept() {
		let mut other_pair = progress("a", TransferDirection::Upload, 20);
		if let Delivery::Event(event) = &mut other_pair {
			event.pair_id = 2;
		}
		let deliveries = vec![
			event(
				6,
				SyncEventMessage::Progress {
					rel_path: "a".to_owned(),
					direction: TransferDirection::Upload,
					bytes: 10,
					total: 100,
				},
			),
			other_pair,
			progress("a", TransferDirection::Upload, 30),
		];

		assert_eq!(coalesce(deliveries.clone()), deliveries);
	}

	fn planned(pass_id: u64) -> Delivery {
		event(pass_id, SyncEventMessage::Planned { actions: 0 })
	}

	fn pass_event(delivery: Delivery) -> SyncPassEvent {
		match delivery {
			Delivery::Event(event) => event,
			Delivery::Status { .. } => panic!("not an event"),
		}
	}

	fn status_call(pair_id: u64) -> ListenerCall {
		match status(pair_id) {
			Delivery::Status { pair_id, status } => ListenerCall::Status { pair_id, status },
			Delivery::Event(_) => panic!("not a status"),
		}
	}

	#[test]
	fn each_run_of_events_is_one_call_and_each_status_its_own() {
		let calls = listener_calls(vec![
			planned(1),
			planned(2),
			status(1),
			status(2),
			planned(3),
		]);

		assert_eq!(
			calls,
			vec![
				ListenerCall::Events(vec![pass_event(planned(1)), pass_event(planned(2))]),
				status_call(1),
				status_call(2),
				ListenerCall::Events(vec![pass_event(planned(3))]),
			]
		);
	}

	#[test]
	fn a_chunk_of_events_only_is_one_call() {
		let calls = listener_calls(vec![planned(1), planned(2), planned(3)]);

		assert_eq!(
			calls,
			vec![ListenerCall::Events(vec![
				pass_event(planned(1)),
				pass_event(planned(2)),
				pass_event(planned(3)),
			])]
		);
	}

	#[test]
	fn an_empty_chunk_makes_no_call() {
		assert_eq!(listener_calls(Vec::new()), Vec::new());
	}

	#[test]
	fn the_thread_delivers_everything_in_order_in_chunks() {
		let (listener, chunks) = recording();
		let sender = spawn_delivery(listener);
		let sent: Vec<Delivery> = (0..1300)
			.map(|pass_id| event(pass_id, SyncEventMessage::Planned { actions: pass_id }))
			.collect();
		for delivery in &sent {
			sender.send(delivery.clone()).unwrap();
		}
		drop(sender);

		let received: Vec<Vec<Delivery>> = chunks.iter().collect();
		assert!(
			received
				.iter()
				.all(|chunk| !chunk.is_empty() && chunk.len() <= DELIVERY_CHUNK),
			"{:?}",
			received.iter().map(Vec::len).collect::<Vec<_>>()
		);
		assert_eq!(received.into_iter().flatten().collect::<Vec<_>>(), sent);
	}

	#[test]
	fn a_drain_is_cut_into_chunks_of_the_limit() {
		let lengths = Mutex::new(Vec::new());
		deliver(
			&|chunk: Vec<Delivery>| lengths.lock().unwrap().push(chunk.len()),
			(0..1100)
				.map(|pass_id| event(pass_id, SyncEventMessage::Planned { actions: 0 }))
				.collect(),
		);

		assert_eq!(lengths.into_inner().unwrap(), vec![512, 512, 76]);
	}

	#[test]
	fn a_panicking_listener_loses_only_its_chunk() {
		let lengths = Mutex::new(Vec::new());
		deliver(
			&|chunk: Vec<Delivery>| {
				let first = {
					let mut lengths = lengths.lock().unwrap();
					lengths.push(chunk.len());
					lengths.len() == 1
				};
				if first {
					panic!("the listener threw");
				}
			},
			(0..600)
				.map(|pass_id| event(pass_id, SyncEventMessage::Planned { actions: 0 }))
				.collect(),
		);

		assert_eq!(lengths.into_inner().unwrap(), vec![512, 88]);
	}

	#[test]
	fn a_swapped_listener_receives_from_the_next_drain() {
		let (first, first_chunks) = recording();
		let sender = spawn_delivery(Arc::clone(&first));
		sender.send(status(1)).unwrap();
		assert_eq!(first_chunks.recv().unwrap(), vec![status(1)]);

		let (second, second_chunks) = recording();
		let replacement = Arc::clone(&*second.lock().unwrap());
		*first.lock().unwrap() = replacement;
		sender.send(status(2)).unwrap();
		drop(sender);

		assert_eq!(second_chunks.recv().unwrap(), vec![status(2)]);
		assert!(
			first_chunks.recv().is_err(),
			"the replaced listener must receive nothing more"
		);
	}
}
