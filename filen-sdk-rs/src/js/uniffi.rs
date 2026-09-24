use std::{borrow::Cow, str::FromStr};

use filen_types::serde::rsa::RsaDerPublicKey;
use rsa::RsaPublicKey;
use tokio::sync::{
	mpsc::{self, UnboundedSender},
	oneshot,
};

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

#[cfg(test)]
mod tests {
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
}
