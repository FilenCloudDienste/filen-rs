//! What archive jobs may use: how many run at once and their codec's memory.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
	auth::http::ClientConfig,
	fs::archive::retained::{RetainedPool, process_pool},
	job::{JobControl, Stopped, report::Ops},
};

/// Defaults by target. Codec memory bounds the decoder's own state (a dictionary, a window), the
/// member cap every structure kept per entry.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 128 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 250_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 16 << 20;
	pub(crate) const RETAINED_BUDGET: u64 = 48 << 20;
}
#[cfg(target_os = "ios")]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 128 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 1_000_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 32 << 20;
	pub(crate) const RETAINED_BUDGET: u64 = 64 << 20;
}
#[cfg(target_os = "android")]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 192 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 1_000_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 32 << 20;
	pub(crate) const RETAINED_BUDGET: u64 = 64 << 20;
}
#[cfg(not(any(
	all(target_family = "wasm", target_os = "unknown"),
	target_os = "ios",
	target_os = "android"
)))]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 256 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 2;
	pub(crate) const MAX_MEMBERS: u64 = 1_000_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 32 << 20;
	pub(crate) const RETAINED_BUDGET: u64 = 128 << 20;
}

pub(crate) use defaults::{CODEC_MEM_BUDGET, JOB_CONCURRENCY, RETAINED_BUDGET};

/// The smallest codec budget: enough for every decoder at its default settings.
const MIN_CODEC_MEM_BUDGET: u64 = 16 << 20;

/// The archive settings a client runs its jobs under, built from its [`ClientConfig`]. Never
/// fails: out-of-range settings are clamped.
///
/// On wasm there is one codec worker per page, so one job runs at a time per page whatever the
/// client (a web worker builds a client per public link): the lease is process-wide there.
#[derive(Clone)]
pub struct ArchiveConfig {
	/// See [`ArchiveConfig::codec_mem_budget`].
	codec_mem_budget: u64,
	/// See [`ArchiveConfig::job_concurrency`].
	job_concurrency: usize,
	/// See [`ArchiveConfig::retained_budget`].
	retained_budget: u64,
	/// What every archive reader of the process keeps between its calls, charged against
	/// `retained_budget`.
	pub(crate) retained: Arc<RetainedPool>,
	/// Most members an archive may have, every tar record counted, and most directories an
	/// extraction plans, those only implied by the paths below them counted too, as are the
	/// folders in `__MACOSX` folders when macOS metadata is left out. Extracting or
	/// listing a tar keeps up to 96 bytes per member for the hard links after it: a file they
	/// may name, or anything else at a path, which keeps them from naming an earlier file there
	/// (96 MB for a million members), outside the codec's memory. Fixed per target, not a
	/// setting.
	pub(crate) max_members: u64,
	/// Most bytes of an archive's index (a zip's central directory, a 7z's header) read into
	/// memory. Fixed per target, not a setting.
	pub(crate) max_index_bytes: u64,
	gate: Arc<Semaphore>,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
static PAGE_GATE: std::sync::LazyLock<Arc<Semaphore>> =
	std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));

impl ArchiveConfig {
	/// Every construction path (default, builder, `From<JsClientConfig>`) ends in this one
	/// constructor, so the clamps live here.
	pub(crate) fn new(config: &ClientConfig) -> Self {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		let (job_concurrency, gate) = (1, Arc::clone(&PAGE_GATE));
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let (job_concurrency, gate) = {
			// a zero-permit gate parks every job forever, and tokio's Semaphore panics above
			// MAX_PERMITS
			let concurrency = config
				.archive_job_concurrency
				.clamp(1, Semaphore::MAX_PERMITS);
			(concurrency, Arc::new(Semaphore::new(concurrency)))
		};
		Self {
			codec_mem_budget: config.archive_codec_mem_budget.max(MIN_CODEC_MEM_BUDGET),
			job_concurrency,
			// lowered only: the pool is the process's, and its default the platform's ceiling
			retained_budget: config.archive_retained_budget.min(RETAINED_BUDGET),
			retained: process_pool(),
			max_members: defaults::MAX_MEMBERS,
			max_index_bytes: defaults::MAX_INDEX_BYTES,
			gate,
		}
	}

	/// Memory for one job's codec state, in bytes: an extraction whose decoder needs more fails
	/// with [`ErrorKind::ArchiveTooLarge`], a compression whose encoder needs more with
	/// [`ErrorKind::InsufficientMemory`].
	///
	/// A RAR's headers are parsed into a quarter of it at most, which bounds how many members
	/// (and service headers) a RAR may have, below the member limit of a zip or 7z: about
	/// `codec_mem_budget ÷ 4 ÷ 1104` for a RAR 5, `÷ 496` for a RAR 1.5 to 4 and `÷ 256` for a
	/// RAR 1.3. With the default budget that is 30 393, 67 650 and 131 072 on the web and iOS
	/// (128 MiB), 45 590, 101 475 and 196 608 on Android (192 MiB), and 60 787, 135 300 and
	/// 262 144 elsewhere (256 MiB). A RAR with more fails with [`ErrorKind::ArchiveTooLarge`],
	/// saying so.
	///
	/// [`ErrorKind::ArchiveTooLarge`]: crate::ErrorKind::ArchiveTooLarge
	/// [`ErrorKind::InsufficientMemory`]: crate::ErrorKind::InsufficientMemory
	pub fn codec_mem_budget(&self) -> u64 {
		self.codec_mem_budget
	}

	/// Archive jobs that run at once, at least 1 (1 on wasm, per page). A running job keeps its
	/// slot while paused (its codec state stays resident); a job paused before it got one waits
	/// without taking it.
	pub fn job_concurrency(&self) -> usize {
		self.job_concurrency
	}

	/// Memory, in bytes, that archive readers keep between their calls: indexes read, and the
	/// archive chunks read last. It is shared by every reader of the process: a reader that needs
	/// more takes it from the readers used least recently, which then read again what they gave
	/// up. 48 MiB on the web, 64 MiB on iOS and Android, 128 MiB elsewhere, or less when the
	/// client's config asks for less; 0 keeps nothing.
	pub fn retained_budget(&self) -> u64 {
		self.retained_budget
	}

	/// Waits for a job slot, held until dropped. A pause stops the wait: the job waits the pause
	/// out reported paused, holding no slot, and gives back one granted as the pause came, so a
	/// paused job never keeps a later one from running. `Err` once the job is stopping.
	pub(crate) async fn admit(
		&self,
		control: &JobControl,
		ops: &Ops,
	) -> Result<OwnedSemaphorePermit, Stopped> {
		loop {
			ops.checkpoint(control).await?;
			let lease = tokio::select! {
				biased;
				() = control.stopping() => return Err(Stopped),
				() = control.pause_changed(false) => None,
				lease = self.lease() => Some(lease),
			};
			if let Some(lease) = lease
				&& !control.is_pause_requested()
			{
				return Ok(lease);
			}
		}
	}

	/// Waits for a job slot.
	async fn lease(&self) -> OwnedSemaphorePermit {
		Arc::clone(&self.gate)
			.acquire_owned()
			.await
			.expect("the archive job gate is never closed")
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use super::ArchiveConfig;

	impl ArchiveConfig {
		/// Settings whose readers draw on a pool of their own of `budget` bytes, so a test's
		/// readers meet no other test's.
		pub(crate) fn with_own_pool(mut self, budget: u64) -> Self {
			self.retained_budget = budget;
			self.retained = std::sync::Arc::default();
			self
		}

		/// Job slots no job holds.
		pub(crate) fn free_slots(&self) -> usize {
			self.gate.available_permits()
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn settings_are_clamped_rather_than_refused() {
		let config = ArchiveConfig::new(
			&ClientConfig::default()
				.with_archive_codec_mem_budget(0)
				.with_archive_job_concurrency(0),
		);
		assert_eq!(config.codec_mem_budget(), MIN_CODEC_MEM_BUDGET);
		assert_eq!(config.job_concurrency(), 1);
		let config =
			ArchiveConfig::new(&ClientConfig::default().with_archive_job_concurrency(usize::MAX));
		assert_eq!(config.job_concurrency(), Semaphore::MAX_PERMITS);
		assert_eq!(config.free_slots(), Semaphore::MAX_PERMITS);
	}

	#[test]
	fn the_retained_budget_can_be_lowered_but_not_raised() {
		let lowered =
			ArchiveConfig::new(&ClientConfig::default().with_archive_retained_budget(5 << 20));
		assert_eq!(lowered.retained_budget(), 5 << 20);
		let raised =
			ArchiveConfig::new(&ClientConfig::default().with_archive_retained_budget(u64::MAX));
		assert_eq!(raised.retained_budget(), RETAINED_BUDGET);
		let none = ArchiveConfig::new(&ClientConfig::default().with_archive_retained_budget(0));
		assert_eq!(none.retained_budget(), 0);
	}
}
