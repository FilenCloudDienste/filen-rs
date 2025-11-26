use std::{borrow::Cow, ops::Deref, time::Duration};

use stable_deref_trait::StableDeref;

use crate::{Error, socket::events::DecryptedSocketEvent};

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

pub(super) trait Socket<T, U, S, R, RV, US, UR, PT>: Sized
where
	S: Sender,
	R: Receiver<RV>,
	RV: IntoStableDeref,
	<RV::Output as Deref>::Target: AsRef<str>,
	US: UnauthedSender<AuthedType = S>,
	UR: UnauthedReceiver<RV, AuthedType = R>,
	U: UnauthedSocket<US, UR, RV>,
	PT: PingTask<S>,
{
	async fn build_request() -> Result<T, Error>;

	async fn connect(request: T) -> Result<U, Error>;

	fn from_unauthed_parts(unauthed_sender: US, unauthed_receiver: UR) -> Self;

	fn split_into_receiver_and_ping_task(self, ping_interval: Duration) -> (R, PT);
}

pub(super) trait IntoStableDeref
where
	Self::Output: Deref,
	<Self::Output as Deref>::Target: 'static,
{
	type Output: StableDeref + 'static;

	fn into_stable_deref(self) -> Self::Output;
}

pub(super) trait PingTask<S>
where
	S: Sender,
{
	fn new(sender: S, interval_duration: std::time::Duration) -> Self;
	fn abort(self);
}
