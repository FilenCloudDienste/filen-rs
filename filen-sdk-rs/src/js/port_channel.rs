//! The `MessageChannel` between the commander and the thread a JS stream lives on, over which the
//! SDK's stream bridges talk without sharing anything in wasm memory (see
//! [`port_from_worker`](crate::runtime::port_from_worker)).

use futures::{
	StreamExt,
	channel::mpsc::{UnboundedReceiver, unbounded},
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{MessageChannel, MessageEvent, MessagePort};

use crate::{Error, ErrorKind, runtime::PortRequest};

/// The messages a port gets, in the order they came, on this thread.
pub(crate) struct PortMessages {
	port: MessagePort,
	messages: UnboundedReceiver<JsValue>,
	/// Called by the port with each message for as long as this holds it.
	_on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl PortMessages {
	pub(crate) fn new(port: MessagePort) -> Self {
		let (sender, messages) = unbounded();
		let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
			// fails only once this was dropped, which takes the handler off the port first
			let _ = sender.unbounded_send(event.data());
		});
		// which also starts the port
		port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
		Self {
			port,
			messages,
			_on_message: on_message,
		}
	}

	/// The port, to post to the other thread with.
	pub(crate) fn port(&self) -> &MessagePort {
		&self.port
	}

	pub(crate) async fn next(&mut self) -> JsValue {
		self.messages
			.next()
			.await
			.expect("the port's handler, which this holds, holds the sender (should be impossible)")
	}
}

impl Drop for PortMessages {
	fn drop(&mut self) {
		self.port.set_onmessage(None);
	}
}

/// Opens a `MessageChannel` and sends one of its ports to the thread that made `request`, for the
/// messages that thread posts to arrive here, on the other port.
///
/// Fails only when the browser refuses a new channel, or the transfer of a port it just made, and
/// the thread that made `request` is then never told.
pub(crate) fn open(request: PortRequest) -> Result<PortMessages, Error> {
	let channel = MessageChannel::new()
		.map_err(|e| broken(format!("failed to open the stream's channel: {e:?}")))?;
	request
		.send(channel.port2())
		.map_err(|e| broken(format!("failed to send the stream's port: {e:?}")))?;
	Ok(PortMessages::new(channel.port1()))
}

/// The error for an exchange with a stream's thread that broke down, which only a browser
/// refusing what a `MessageChannel` takes can do.
pub(crate) fn broken(message: String) -> Error {
	Error::custom(ErrorKind::Internal, message)
}
