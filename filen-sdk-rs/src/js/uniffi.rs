use std::{borrow::Cow, str::FromStr};

use filen_types::serde::rsa::RsaDerPublicKey;
use rsa::RsaPublicKey;
use tokio::sync::{
	mpsc::{self, UnboundedSender},
	oneshot,
};

use crate::{Error, job::JobControl, js::ManagedFuture};

#[cfg(feature = "uniffi")]
uniffi::use_remote_type!(filen_types::filen_types::fs::UuidStr);

#[cfg(feature = "uniffi")]
uniffi::use_remote_type!(filen_types::uuid::Uuid);

#[cfg(feature = "uniffi")]
uniffi::custom_type!(RsaPublicKey, String, {
	remote,
	lower: |key: &RsaPublicKey| RsaDerPublicKey(Cow::Borrowed(&key)).to_string(),
	try_lift: |s: &str| {
		RsaDerPublicKey::from_str(&s).map(|k| k.0.into_owned()).map_err(|e| uniffi::deps::anyhow::anyhow!("failed to parse RSA public key from string: {}", e))
	},
});

/// Delivers items to `handler` on a single dedicated thread in the exact order they were sent.
///
/// UniFFI foreign callbacks must run off the async runtime (a slow implementation must not stall
/// the cache, the search engine or a copy), but dispatching each invocation with its own
/// `spawn_blocking` gave NO ordering guarantee — two back-to-back items could land on different
/// pool threads and run reversed (e.g. `ResyncProgress::Finished` before the final `Listing`, or a
/// stale search snapshot delivered last). One channel with one consumer restores emission order
/// while keeping the foreign call off the runtime. A plain thread (not a runtime task) hosts the
/// consumer, so `blocking_recv` is valid and no ambient Tokio runtime is required at the call
/// site. Dropping every returned sender ends the consumer, which then resolves the returned
/// receiver: every item sent by then has been handled.
pub(crate) fn spawn_ordered_dispatch<T, F>(
	mut handler: F,
) -> (UnboundedSender<T>, oneshot::Receiver<()>)
where
	T: Send + 'static,
	F: FnMut(T) + Send + 'static,
{
	let (sender, mut receiver) = mpsc::unbounded_channel::<T>();
	let (done, handled) = oneshot::channel();
	crate::runtime::spawn(move || {
		while let Some(item) = receiver.blocking_recv() {
			handler(item);
		}
		let _ = done.send(());
	});
	(sender, handled)
}

impl ManagedFuture {
	/// Runs `job` as [`into_js_managed_commander_job`](Self::into_js_managed_commander_job)
	/// does, handing it a channel whose items reach `deliver` in the order they were sent, on
	/// the ordered dispatch thread (a foreign callback may block). Resolves once the job has
	/// ended and everything it sent was delivered. Waiting for the callbacks is no part of the
	/// job, so a cancel's grace never cuts off a report already made.
	pub(crate) async fn into_ordered_job<D, T, F, Fut>(
		self,
		deliver: impl FnMut(D) + Send + 'static,
		job: F,
	) -> Result<T, Error>
	where
		D: Send + 'static,
		F: FnOnce(UnboundedSender<D>, JobControl) -> Fut + Send + 'static,
		Fut: Future<Output = Result<T, Error>> + Send + 'static,
		T: Send + 'static,
	{
		let (sender, delivered) = spawn_ordered_dispatch(deliver);
		let result = self
			.into_js_managed_commander_job(move |control| job(sender, control))
			.await;
		// the job has ended and dropped its sender: this returns once everything it sent was
		// delivered
		let _ = delivered.await;
		result
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	/// The ordered dispatch task must deliver every item to the foreign handler in the exact
	/// order it was sent — the whole point of replacing per-call `spawn_blocking`, which could
	/// reorder back-to-back items across pool threads — and say when it has handled them all.
	#[test]
	fn ordered_dispatch_preserves_emission_order() {
		let (out_tx, out_rx) = std::sync::mpsc::channel::<u64>();
		let (sender, handled) = spawn_ordered_dispatch(move |i: u64| {
			out_tx.send(i).expect("test receiver still alive");
		});
		for i in 0..1000u64 {
			sender.send(i).expect("dispatch consumer alive");
		}
		// Closing the channel lets the consumer drain, then signal.
		drop(sender);
		futures::executor::block_on(handled).expect("the consumer signals once drained");
		let received: Vec<u64> = out_rx.try_iter().collect();
		assert_eq!(received, (0..1000).collect::<Vec<u64>>());
	}

	/// A job's result waits for every callback it sent, however slow the foreign side is: an
	/// app taking the result as the end of the job never gets a callback after it.
	#[test]
	fn an_ordered_job_resolves_once_everything_it_sent_was_delivered() {
		let (open_gate, gate) = std::sync::mpsc::channel::<()>();
		let (ended_tx, ended) = std::sync::mpsc::channel::<()>();
		let (delivered_tx, delivered) = std::sync::mpsc::channel::<u64>();
		let managed = ManagedFuture {
			abort_signal: None,
			pause_signal: None,
		};
		let job = managed.into_ordered_job(
			move |i: u64| {
				// the first callback blocks, as a slow foreign one would
				if i == 0 {
					gate.recv().expect("the test opens the gate");
				}
				delivered_tx.send(i).expect("the test still listens");
			},
			move |sender, _control| async move {
				for i in 0..100 {
					sender.send(i).expect("the dispatch thread is alive");
				}
				ended_tx.send(()).expect("the test still listens");
				Ok::<_, Error>(7)
			},
		);
		let (result_tx, result) = std::sync::mpsc::channel();
		std::thread::spawn(move || result_tx.send(futures::executor::block_on(job)));

		ended.recv().expect("the job runs to its end");
		assert!(
			result.recv_timeout(Duration::from_millis(200)).is_err(),
			"the result waits for the blocked callback"
		);
		open_gate
			.send(())
			.expect("the first callback waits on the gate");
		let result = result
			.recv()
			.expect("the call resolves once all is delivered");
		assert_eq!(result.expect("the job's own result"), 7);
		assert_eq!(
			delivered.try_iter().collect::<Vec<u64>>(),
			(0..100).collect::<Vec<_>>()
		);
	}
}
