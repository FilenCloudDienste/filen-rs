//! What the binding tests share: a record of what a job delivered.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::{Error, js::ManagedFuture};

/// Records what a job delivered, each callback by a number it carries, in the order it came.
#[derive(Default)]
pub(crate) struct Recorder(Mutex<Vec<u64>>);

impl Recorder {
	pub(crate) fn push(&self, value: u64) {
		self.0.lock().unwrap().push(value);
	}
}

/// Runs a job that sends what `send` sends (returning the numbers the callbacks record, in the
/// order sent) through [`ManagedFuture::into_ordered_job`], each delivered through `deliver`,
/// and checks it all reached the callbacks, in order, by the time the call resolved.
pub(crate) fn delivered_in_order<T: Send + 'static>(
	deliver: fn(&Recorder, T),
	send: impl FnOnce(UnboundedSender<T>) -> Vec<u64> + Send + 'static,
) {
	let recorder = Arc::new(Recorder::default());
	let managed = ManagedFuture {
		abort_signal: None,
		pause_signal: None,
	};
	let job = managed.into_ordered_job(
		{
			let recorder = Arc::clone(&recorder);
			move |delivery| deliver(&recorder, delivery)
		},
		move |sender, _control| async move { Ok::<_, Error>(send(sender)) },
	);
	let expected = futures::executor::block_on(job).expect("the job's own result");
	assert_eq!(*recorder.0.lock().unwrap(), expected);
}
