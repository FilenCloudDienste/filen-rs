use std::{borrow::Cow, ops::Deref, time::Duration};

use stable_deref_trait::StableDeref;
use tokio::sync::mpsc;

use crate::{Error, auth::http::AuthClient, socket::events::DecryptedSocketEvent};

use super::writer::Writer;

pub type EventListenerCallback = Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>;

pub(super) trait Receiver<RV>
where
	RV: IntoStableDeref,
	<RV::Output as Deref>::Target: AsRef<str>,
{
	async fn receive(&mut self) -> Option<Result<RV, Error>>;
}

pub(super) trait UnauthedReceiver<RV>: Receiver<RV>
where
	RV: IntoStableDeref,
	<RV::Output as Deref>::Target: AsRef<str>,
{
	type AuthedType: Receiver<RV>;
}

pub(super) trait Sender {
	async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error>;

	async fn send_multiple(
		&mut self,
		msgs: impl IntoIterator<Item = Cow<'_, str>>,
	) -> Result<Option<()>, Error>;
}

pub(super) trait UnauthedSender: Sender {
	type AuthedType: Sender;
}

pub(super) trait UnauthedSocket<S, R, RV>
where
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref,
	<RV::Output as Deref>::Target: AsRef<str>,
{
	fn split(self) -> (S, R);
}

pub(super) trait Socket<T, U, S, R, RV, US, UR, WT>: Sized
where
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref,
	<RV::Output as Deref>::Target: AsRef<str>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	U: UnauthedSocket<US, UR, RV>,
	WT: WriterTask<Sender = S>,
{
	async fn build_request(client: &AuthClient) -> Result<T, Error>;

	async fn connect(request: T) -> Result<U, Error>;

	/// Whether `request` connects through a proxy, which a failure's log says.
	fn proxied(_request: &T) -> bool {
		false
	}

	fn from_unauthed_parts(unauthed_sender: US, unauthed_receiver: UR) -> Self;

	/// The authed connection's two halves.
	fn into_parts(self) -> (S, R);

	/// The receiving half, and a writer task that owns the sending half and pings every
	/// `ping_interval`.
	fn split_into_receiver_and_writer(self, ping_interval: Duration) -> (R, Writer<WT>) {
		let (sender, receiver) = self.into_parts();
		(receiver, Writer::spawn(sender, ping_interval))
	}
}

pub(super) trait IntoStableDeref
where
	Self::Output: Deref + Send,
	<Self::Output as Deref>::Target: 'static + Send,
{
	type Output: StableDeref + 'static;

	fn into_stable_deref(self) -> Self::Output;
}

/// A spawned task running [`super::writer::run`] over a connection's authed sender.
pub(super) trait WriterTask {
	type Sender: Sender;

	fn spawn(
		sender: Self::Sender,
		ping_interval: Duration,
		outbound: mpsc::Receiver<String>,
	) -> Self;
	/// Stops the task at once, dropping the sender and every frame still queued.
	fn abort(self);
}
