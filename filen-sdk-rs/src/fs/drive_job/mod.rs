//! Machinery shared by jobs that turn drive items into new drive items: copies, compressed
//! archives and extracted entries.

pub(crate) mod backend;
pub(crate) mod counts;
pub(crate) mod dir;
pub(crate) mod finalize;
pub(crate) mod listing;
pub(crate) mod lock;
pub(crate) mod name_retry;
pub(crate) mod plan;
#[cfg(test)]
pub(crate) mod test_support;

use std::sync::Arc;

use crate::{
	Error, ErrorKind,
	job::{
		JobControl, Stopped,
		report::{JobPhase, JobTick},
	},
};

/// Errors after which nothing else can succeed either, so they end the whole job.
pub(crate) fn ends_job(error: &Error) -> bool {
	matches!(
		error.kind(),
		ErrorKind::MaxStorageReached | ErrorKind::Unauthenticated
	)
}

/// The error a job ends with once it is cancelled: `job` is what its report calls it.
pub(crate) fn cancelled(job: &str) -> Arc<Error> {
	Arc::new(Error::custom(
		ErrorKind::Cancelled,
		format!("{job} cancelled"),
	))
}

/// The error that ends a job early, recorded in one place: the first one recorded is the job's,
/// and any after it only fail their own items.
#[derive(Debug, Default)]
pub(crate) struct Fatal(Option<Arc<Error>>);

impl Fatal {
	/// Read by the archive jobs, which the service-worker build leaves out.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(crate) fn error(&self) -> Option<&Arc<Error>> {
		self.0.as_ref()
	}

	/// Records `error` as the job's unless one already is, without stopping it: what the job has
	/// in hand may still finish (an extraction's files whose data is whole, when the rest of the
	/// archive turns out damaged).
	pub(crate) fn record(&mut self, error: Arc<Error>) {
		self.0.get_or_insert(error);
	}

	/// Ends the job with `error`, unless an earlier error already did: nothing new starts, and it
	/// reports itself winding down.
	pub(crate) fn stop(&mut self, error: Arc<Error>, control: &JobControl, job: &impl JobTick) {
		self.record(error);
		control.stop();
		job.set_cancelling();
	}

	/// Ends the job with an item's `error` when nothing else can succeed after it ([`ends_job`])
	/// and no earlier error did.
	pub(crate) fn note(&mut self, error: &Arc<Error>, control: &JobControl, job: &impl JobTick) {
		if self.0.is_none() && ends_job(error) {
			self.stop(Arc::clone(error), control, job);
		}
	}

	/// The phase a job ends in once its work came back with `outcome`, and its result: failed
	/// with the error recorded whatever the outcome, cancelled, or done. `job` is what its report
	/// calls it, for the messages.
	pub(crate) fn end<T, P: JobPhase>(
		&self,
		outcome: Result<T, Stopped>,
		control: &JobControl,
		job: &str,
	) -> (P, Result<T, Arc<Error>>) {
		match (outcome, &self.0) {
			(_, Some(error)) => (P::FAILED, Err(Arc::clone(error))),
			(Err(Stopped), None) if control.is_cancelled() => (P::CANCELLED, Err(cancelled(job))),
			// stopped with nothing recorded is a bug: it still ends as failed, never as done
			(Err(Stopped), None) => (
				P::FAILED,
				Err(Arc::new(Error::custom(
					ErrorKind::Internal,
					format!("{job} stopped"),
				))),
			),
			(Ok(value), None) => (P::DONE, Ok(value)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{fs::archive::extract::ExtractPhase, job::test_support::controls};

	#[test]
	fn the_first_error_recorded_ends_the_job_whatever_its_work_returned() {
		let control = JobControl::default();
		let mut fatal = Fatal::default();
		let (phase, result) = fatal.end::<_, ExtractPhase>(Ok(7), &control, "job");
		assert_eq!((phase, result.unwrap()), (ExtractPhase::Done, 7));

		fatal.record(Arc::new(Error::custom(
			ErrorKind::MaxStorageReached,
			"full",
		)));
		fatal.record(Arc::new(Error::custom(ErrorKind::Server, "later")));
		let (phase, result) = fatal.end::<_, ExtractPhase>(Ok(()), &control, "job");
		assert_eq!(phase, ExtractPhase::Failed);
		assert_eq!(result.unwrap_err().kind(), ErrorKind::MaxStorageReached);
	}

	#[test]
	fn a_job_stopped_without_an_error_ended_cancelled_or_failed() {
		let (_pause, cancel, control) = controls();
		let (phase, result) =
			Fatal::default().end::<(), ExtractPhase>(Err(Stopped), &control, "job");
		assert_eq!(phase, ExtractPhase::Failed);
		assert_eq!(result.unwrap_err().kind(), ErrorKind::Internal);

		cancel.send_replace(true);
		let (phase, result) =
			Fatal::default().end::<(), ExtractPhase>(Err(Stopped), &control, "job");
		assert_eq!(phase, ExtractPhase::Cancelled);
		let error = result.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::Cancelled);
		assert!(error.to_string().contains("job cancelled"), "{error}");
	}
}
