//! What archive jobs may use: how many run at once, their codec's memory, and the memory floor
//! each job holds so it can always make progress.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
	consts::{CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA_USIZE},
	job::{JobControl, Stopped, report::Ops},
};

/// Bytes one chunk takes in memory, with its encryption overhead.
pub(crate) const CHUNK_BYTES: usize = CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE;

/// Chunks each running job holds for as long as it runs: one of the archive's input and one of
/// its output. With them, a job never waits on memory another job or a transfer holds.
pub(crate) const FLOOR_CHUNKS: usize = 2;

/// Defaults by target. Codec memory bounds the decoder's own state (a dictionary, a window), the
/// member cap every structure kept per entry.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 128 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 250_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 16 << 20;
}
#[cfg(target_os = "ios")]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 128 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 1_000_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 32 << 20;
}
#[cfg(target_os = "android")]
mod defaults {
	pub(crate) const CODEC_MEM_BUDGET: u64 = 192 << 20;
	pub(crate) const JOB_CONCURRENCY: usize = 1;
	pub(crate) const MAX_MEMBERS: u64 = 1_000_000;
	pub(crate) const MAX_INDEX_BYTES: u64 = 32 << 20;
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
}

pub(crate) use defaults::{CODEC_MEM_BUDGET, JOB_CONCURRENCY};

/// The smallest codec budget: enough for every decoder at its default settings.
const MIN_CODEC_MEM_BUDGET: u64 = 16 << 20;

/// The archive settings a client runs its jobs under, built from its
/// [`ClientConfig`](crate::auth::http::ClientConfig). Never fails: out-of-range settings are
/// clamped.
///
/// On wasm there is one codec worker per page, so one job runs at a time per page whatever the
/// client (a web worker builds a client per public link): the lease is process-wide there.
#[derive(Clone)]
pub struct ArchiveConfig {
	/// Memory for one job's codec state.
	pub codec_mem_budget: u64,
	/// Archive jobs that run at once. A running job keeps its slot while paused (its codec
	/// state stays resident); a job paused before it got one waits without taking it.
	pub job_concurrency: usize,
	/// Most members an archive may have, every tar record counted.
	pub max_members: u64,
	/// Most bytes of an archive's index (a zip's central directory) read into memory.
	pub max_index_bytes: u64,
	gate: Arc<Semaphore>,
	/// [`FLOOR_CHUNKS`] chunks per concurrent job.
	floor: Arc<Semaphore>,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
static PAGE_GATE: std::sync::LazyLock<Arc<Semaphore>> =
	std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));

impl ArchiveConfig {
	pub(crate) fn new(codec_mem_budget: u64, job_concurrency: usize) -> Self {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		let (job_concurrency, gate) = {
			let _ = job_concurrency;
			(1, Arc::clone(&PAGE_GATE))
		};
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let (job_concurrency, gate) = {
			// a zero-permit gate parks every job forever; tokio panics above MAX_PERMITS
			let concurrency = job_concurrency.clamp(1, 64);
			(concurrency, Arc::new(Semaphore::new(concurrency)))
		};
		Self {
			codec_mem_budget: codec_mem_budget.max(MIN_CODEC_MEM_BUDGET),
			job_concurrency,
			max_members: defaults::MAX_MEMBERS,
			max_index_bytes: defaults::MAX_INDEX_BYTES,
			gate,
			floor: Arc::new(Semaphore::new(job_concurrency * FLOOR_CHUNKS * CHUNK_BYTES)),
		}
	}

	/// Waits for a job slot and then its memory floor, both held until dropped. A pause stops
	/// the wait: the job waits the pause out reported paused, holding neither, and gives back a
	/// slot granted as the pause came, so a paused job never keeps a later one from running.
	/// `Err` once the job is stopping.
	pub(crate) async fn admit(
		&self,
		control: &JobControl,
		ops: &Ops,
	) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), Stopped> {
		loop {
			ops.set_pause_requested(control.is_pause_requested());
			control.checkpoint().await?;
			ops.set_pause_requested(false);
			let admitted = tokio::select! {
				biased;
				() = control.stopping() => return Err(Stopped),
				() = control.pause_changed(false) => None,
				admitted = async { (self.lease().await, self.floor().await) } => Some(admitted),
			};
			if let Some(admitted) = admitted
				&& !control.is_pause_requested()
			{
				return Ok(admitted);
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

	/// Waits for a job's memory floor. Only jobs holding a lease take it, so it is always handed
	/// out soon.
	pub(crate) async fn floor(&self) -> OwnedSemaphorePermit {
		Arc::clone(&self.floor)
			.acquire_many_owned((FLOOR_CHUNKS * CHUNK_BYTES) as u32)
			.await
			.expect("the archive memory floor is never closed")
	}
}

#[cfg(test)]
impl ArchiveConfig {
	/// Job slots no job holds.
	pub(crate) fn free_slots(&self) -> usize {
		self.gate.available_permits()
	}

	/// Whether no job holds its memory floor.
	pub(crate) fn floor_is_free(&self) -> bool {
		self.floor.available_permits() == self.job_concurrency * FLOOR_CHUNKS * CHUNK_BYTES
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn settings_are_clamped_rather_than_refused() {
		let config = ArchiveConfig::new(0, 0);
		assert_eq!(config.codec_mem_budget, MIN_CODEC_MEM_BUDGET);
		assert_eq!(config.job_concurrency, 1);
		assert_eq!(config.floor.available_permits(), FLOOR_CHUNKS * CHUNK_BYTES);
		let config = ArchiveConfig::new(1 << 30, 1000);
		assert_eq!(config.job_concurrency, 64);
	}

	#[tokio::test]
	async fn every_leased_job_gets_its_floor() {
		let config = ArchiveConfig::new(CODEC_MEM_BUDGET, 2);
		let (_a, _b) = (config.lease().await, config.lease().await);
		let (_fa, _fb) = (config.floor().await, config.floor().await);
		assert_eq!(config.floor.available_permits(), 0);
	}
}
