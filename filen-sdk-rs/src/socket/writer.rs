use std::{borrow::Cow, time::Duration};

use tokio::sync::mpsc;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::interval;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::interval;

use super::{
	consts::{OUTBOUND_FRAME_CAPACITY, PING_MESSAGE},
	traits::{Sender, WriterTask},
};

/// The connected loop's side of the task that owns a connection's authed sender.
///
/// The task sends the pings every `ping_interval` and the frames queued here, in the order they
/// were queued. Aborting it discards whatever is still queued: a frame never outlives the
/// connection it was queued for.
pub(super) struct Writer<WT> {
	task: WT,
	outbound: mpsc::Sender<String>,
}

impl<WT: WriterTask> Writer<WT> {
	pub(super) fn spawn(sender: WT::Sender, ping_interval: Duration) -> Self {
		let (outbound, outbound_receiver) = mpsc::channel(OUTBOUND_FRAME_CAPACITY);
		Self {
			task: WT::spawn(sender, ping_interval, outbound_receiver),
			outbound,
		}
	}

	/// Queues `frame` behind the frames queued before it, failing at once when the queue is full
	/// or the task has ended.
	// Only the tests queue frames so far; the connected loop has nothing of its own to send yet.
	#[cfg_attr(
		not(test),
		expect(dead_code, reason = "no frame producer yet outside tests")
	)]
	pub(super) fn try_queue(&self, frame: String) -> Result<(), mpsc::error::TrySendError<String>> {
		self.outbound.try_send(frame)
	}

	pub(super) fn abort(self) {
		self.task.abort();
	}
}

/// The writer task's body: a ping every `ping_interval`, starting at once, and every frame from
/// `outbound`. It ends when the connection is closed or when the [`Writer`] is dropped.
pub(super) async fn run<S: Sender>(
	mut sender: S,
	ping_interval: Duration,
	mut outbound: mpsc::Receiver<String>,
) {
	let mut interval = interval(ping_interval);

	loop {
		let (what, sent) = tokio::select! {
			biased;
			_ = interval.tick() => {
				let sent = sender
					.send_multiple([
						Cow::Borrowed(PING_MESSAGE),
						Cow::Owned(format!(
							r#"42["authed", {}]"#,
							chrono::Utc::now().timestamp_millis()
						)),
					])
					.await;
				("ping", sent)
			}
			frame = outbound.recv() => {
				let Some(frame) = frame else {
					tracing::debug!("WebSocket writer dropped, stopping writer task");
					break;
				};
				("frame", sender.send(Cow::Owned(frame)).await)
			}
		};
		match sent {
			Ok(Some(())) => {}
			Ok(None) => {
				tracing::debug!("WebSocket has been closed, stopping writer task");
				break;
			}
			Err(e) => tracing::warn!("Failed to send WebSocket {what}: {e}"),
		}
	}
}
