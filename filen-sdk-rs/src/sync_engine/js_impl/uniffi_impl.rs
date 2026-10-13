//! The sync engine's UniFFI exports.

use std::{
	panic::{AssertUnwindSafe, catch_unwind},
	sync::Arc,
};

use super::{
	JsSyncEngine,
	delivery::{Delivery, Listener, ListenerCall, listener_calls},
	types::{SyncPassEvent, SyncRefusal, sync_refusal},
};
use crate::{
	Error,
	auth::JsClient,
	error::FilenSdkError,
	sync_engine::{DEFAULT_IGNORE_PATTERNS, WatchStatus},
};

/// Receives every pair's pass events and watch statuses, in the order they happened, on one
/// delivery thread, never the thread running the pass. Must return quickly. A call that throws is
/// logged and loses only the deliveries it carried: the rest of its chunk is still delivered.
#[uniffi::export(with_foreign)]
pub trait SyncEngineListener: Send + Sync {
	/// A run of consecutive pass events, at most 512.
	fn on_events(&self, events: Vec<SyncPassEvent>);
	/// A watch's status changed (and once when the watch starts).
	fn on_watch_status(&self, pair_id: u64, status: WatchStatus);
}

/// The delivery thread's listener around `listener`: each run of consecutive events is one
/// `on_events` call, each status one `on_watch_status` call, so the two keep their order. Each call
/// is guarded on its own, so one that throws does not lose the rest of the chunk.
fn listener_closure(listener: Arc<dyn SyncEngineListener>) -> Listener {
	Arc::new(move |deliveries: Vec<Delivery>| {
		for call in listener_calls(deliveries) {
			match call {
				ListenerCall::Events(events) => guarded("on_events", || listener.on_events(events)),
				ListenerCall::Status { pair_id, status } => guarded("on_watch_status", || {
					listener.on_watch_status(pair_id, status)
				}),
			}
		}
	})
}

/// Make one foreign listener call, logging a panic (UniFFI panics when a JS callback throws)
/// instead of letting it cut the chunk short.
fn guarded(method: &str, call: impl FnOnce()) {
	if catch_unwind(AssertUnwindSafe(call)).is_err() {
		tracing::error!("the sync engine listener's {method} threw; its deliveries were lost");
	}
}

#[uniffi::export]
impl JsClient {
	/// Open the sync engine on the baseline DB at `db_path`, whose directory must exist; call
	/// `configure_cache` first, or remote changes are not announced. `listener` receives every
	/// pair's events and watch statuses.
	///
	/// If this process already has the DB open for the same account (a JS reload), this attaches
	/// to that engine instead: its watches keep running, and `listener` replaces the old one from
	/// the next delivery round. The old one still receives the rest of the round in progress. If
	/// it has it open for another account, or is still shutting it down, or another process has
	/// it open, this fails with the `BaselineInUse` sync refusal.
	/// On Android nothing guards across processes: open it in one process only.
	pub async fn open_sync_engine(
		&self,
		db_path: String,
		listener: Arc<dyn SyncEngineListener>,
	) -> Result<JsSyncEngine, Error> {
		JsSyncEngine::open(self.inner(), db_path, listener_closure(listener)).await
	}
}

// Named by the struct, not the `Error` alias: UniFFI registers these methods under the name it is
// given, and the object JS knows is `FilenSdkError`.
#[uniffi::export]
impl FilenSdkError {
	/// The sync refusal this error is, for a UI to act on without reading the message: a baseline
	/// another sync engine has open, or a pair whose roots overlap an existing one's. `None` for
	/// every other error.
	pub fn sync_refusal(&self) -> Option<SyncRefusal> {
		sync_refusal(self)
	}
}

/// The ignore patterns every sync pair applies below its own, for an ignore editor to show as
/// built in.
#[uniffi::export]
pub fn default_sync_ignore_patterns() -> Vec<String> {
	DEFAULT_IGNORE_PATTERNS
		.iter()
		.map(|pattern| (*pattern).to_owned())
		.collect()
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;
	use crate::sync_engine::{WatchState, js_impl::types::SyncEventMessage};

	/// One listener call, by the pass ids or pair it carried.
	#[derive(Debug, PartialEq)]
	enum Call {
		Events(Vec<u64>),
		Status(u64),
	}

	/// Records every call, then throws from the first.
	#[derive(Default)]
	struct ThrowsOnce {
		calls: Mutex<Vec<Call>>,
	}

	impl ThrowsOnce {
		fn record(&self, call: Call) {
			let first = {
				let mut calls = self.calls.lock().unwrap();
				calls.push(call);
				calls.len() == 1
			};
			if first {
				panic!("the listener threw");
			}
		}
	}

	impl SyncEngineListener for ThrowsOnce {
		fn on_events(&self, events: Vec<SyncPassEvent>) {
			self.record(Call::Events(
				events.iter().map(|event| event.pass_id).collect(),
			));
		}

		fn on_watch_status(&self, pair_id: u64, _status: WatchStatus) {
			self.record(Call::Status(pair_id));
		}
	}

	fn event(pass_id: u64) -> Delivery {
		Delivery::Event(SyncPassEvent {
			pair_id: 1,
			pass_id,
			emitted_at_ms: 0,
			event: SyncEventMessage::Planned { actions: 0 },
		})
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

	#[test]
	fn a_throwing_call_loses_none_of_the_other_calls_of_its_chunk() {
		let listener = Arc::new(ThrowsOnce::default());
		let closure = listener_closure(Arc::clone(&listener) as Arc<dyn SyncEngineListener>);

		closure(vec![event(1), event(2), status(1), event(3), status(2)]);

		assert_eq!(
			*listener.calls.lock().unwrap(),
			vec![
				Call::Events(vec![1, 2]),
				Call::Status(1),
				Call::Events(vec![3]),
				Call::Status(2),
			]
		);
	}
}
