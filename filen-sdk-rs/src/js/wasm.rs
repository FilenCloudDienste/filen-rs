use serde::Serialize;
use tokio::sync::{
	mpsc::{self, UnboundedSender},
	oneshot,
};
use wasm_bindgen::JsValue;
use web_sys::js_sys;

use crate::{Error, job::JobControl, js::ManagedFuture};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(start)
)]
pub fn main_js() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	#[cfg(debug_assertions)]
	crate::obs::try_init(crate::auth::http::LogLevel::Debug);
	#[cfg(not(debug_assertions))]
	crate::obs::try_init(crate::auth::http::LogLevel::Info);
	Ok(())
}

/// Delivers items to `handler` on this thread in the order they were sent: the wasm twin of the
/// uniffi `spawn_ordered_dispatch`. A job runs on the commander and sends its callbacks here,
/// where the JS functions it calls live (they never leave the thread that got them). Dropping
/// every returned sender ends the task, which then resolves the returned receiver: every item
/// sent by then has been handled.
fn spawn_local_dispatch<T: 'static>(
	mut handler: impl FnMut(T) + 'static,
) -> (UnboundedSender<T>, oneshot::Receiver<()>) {
	let (sender, mut receiver) = mpsc::unbounded_channel::<T>();
	let (done, handled) = oneshot::channel();
	crate::runtime::spawn_local(async move {
		while let Some(item) = receiver.recv().await {
			handler(item);
		}
		let _ = done.send(());
	});
	(sender, handled)
}

impl ManagedFuture {
	/// Runs `job` as [`into_js_managed_commander_job`](Self::into_js_managed_commander_job)
	/// does, handing it a channel whose items reach `deliver` in the order they were sent, on
	/// this thread: the wasm twin of the uniffi `into_ordered_job`. Resolves once the job has
	/// ended and everything it sent was delivered, so every callback runs before the call
	/// resolves.
	pub(crate) async fn into_ordered_job<D, T, F, Fut>(
		self,
		deliver: impl FnMut(D) + 'static,
		job: F,
	) -> Result<T, Error>
	where
		D: Send + 'static,
		F: FnOnce(UnboundedSender<D>, JobControl) -> Fut + Send + 'static,
		Fut: Future<Output = Result<T, Error>> + 'static,
		T: Send + 'static,
	{
		let (sender, delivered) = spawn_local_dispatch(deliver);
		let result = self
			.into_js_managed_commander_job(move |control| job(sender, control))?
			.await;
		// the job has ended and dropped its sender: this returns once everything it sent was
		// delivered
		let _ = delivered.await;
		result
	}
}

/// Calls a job's `callback` with `value`, if the caller passed one, serialized as the SDK's
/// other records are: objects rather than maps, large numbers as BigInt.
pub(crate) fn call_callback(callback: Option<&js_sys::Function>, value: &impl Serialize) {
	let Some(callback) = callback else {
		return;
	};
	let serializer = serde_wasm_bindgen::Serializer::new()
		.serialize_maps_as_objects(true)
		.serialize_large_number_types_as_bigints(true);
	let value = value
		.serialize(&serializer)
		.expect("failed to serialize a job callback (should be impossible)");
	let _ = callback.call1(&JsValue::UNDEFINED, &value);
}
