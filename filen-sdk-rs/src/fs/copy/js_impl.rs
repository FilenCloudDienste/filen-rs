//! `copyItems` / `copyItemsTo` for the wasm and uniffi bindings. Both platforms take the same
//! items and report the same types; the job's callbacks reach the caller in the order the job
//! made them, all before the call returns.

use std::{sync::Arc, time::Duration};

use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		categories::{DirType, Normal},
		copy as api,
		file::enums::RemoteFileType,
	},
	js::{
		AnyDirWithContext, AnyFile, AnyLinkedDirWithContext, AnyNormalDir, AnySharedDirWithContext,
		DirByCategoryWithContext, NonRootNormalItemTagged,
	},
};

use super::{
	ActiveFile, CopyCounts, CopyPhase, CopyStage, PlanTotals, RenameReason, RunState, ScanProgress,
	SkipReason,
};

/// An item to copy: a file, or a directory with what is needed to list it (its share or
/// public link). A copy's failures hand their items back in this form, so they can be passed
/// to `copyItemsTo` again.
#[js_type(import)]
pub enum CopyItem {
	File(AnyFile),
	Dir(AnyDirWithContext),
}

/// One item to copy into its own destination, optionally under another name.
#[js_type(import, no_ser)]
pub struct CopyEntry {
	pub item: CopyItem,
	pub destination: AnyNormalDir,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"),
		serde(default),
		tsify(optional)
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub name: Option<String>,
}

/// An error in a copy's progress or report.
#[js_type(export, no_deser)]
pub struct CopyError {
	pub kind: ErrorKind,
	pub message: String,
	/// The server's message, for errors the server returned.
	pub server_message: Option<String>,
	pub server_code: Option<String>,
}

#[js_type(export, no_deser)]
pub struct CopyFailureInfo {
	pub source_uuid: Uuid,
	pub source_path: String,
	/// The directory the item was to be created in.
	pub dest_parent: Uuid,
	/// The same directory, to retry the item in with `copyItemsTo`.
	pub dest_parent_dir: AnyNormalDir,
	pub dest_name: String,
	pub stage: CopyStage,
	pub error: CopyError,
	/// Files and bytes not copied because of this failure (a directory's whole subtree).
	pub affected_files: u64,
	pub affected_bytes: u64,
	/// With stage `registeredAsVersion`: the file the copy became a version of.
	pub existing_file: Option<Uuid>,
}

#[js_type(export, no_deser)]
pub struct CopyDirCreated {
	pub source_uuid: Uuid,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
}

#[js_type(export, no_deser)]
pub struct CopyFileDone {
	pub source_uuid: Uuid,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
	pub size: u64,
}

#[js_type(export, no_deser)]
pub struct CopySkippedEntry {
	pub source_path: String,
	pub bytes: u64,
	pub reason: SkipReason,
}

#[js_type(export, no_deser)]
pub struct CopyRenamedEntry {
	pub source_uuid: Uuid,
	pub source_path: String,
	pub name: String,
	pub reason: RenameReason,
}

/// A created item that could not get its color, or could not be added to one of the
/// destination's public links or shares.
#[js_type(export, no_deser)]
pub struct CopyItemError {
	pub dest_uuid: Uuid,
	pub error: CopyError,
}

#[js_type(export, no_deser, tagged)]
pub enum CopyEvent {
	DirCreated(CopyDirCreated),
	DirFailed(CopyFailureInfo),
	FileStarted(ActiveFile),
	FileDone(CopyFileDone),
	FileFailed(CopyFailureInfo),
	Skipped(CopySkippedEntry),
	Renamed(CopyRenamedEntry),
	PropagationFailed(CopyItemError),
	ColorFailed(CopyItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[js_type(export, no_deser)]
pub struct CopyUpdate {
	pub phase: CopyPhase,
	pub run_state: RunState,
	pub scan: ScanProgress,
	pub totals: PlanTotals,
	pub counts: CopyCounts,
	pub active: Vec<ActiveFile>,
	pub events: Vec<CopyEvent>,
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds.
	pub eta_ms: Option<u64>,
	/// Time spent running, paused time left out, in milliseconds.
	pub active_time_ms: u64,
}

/// A top-level item as planned, announced before anything is created so a caller can clean up
/// even after an abrupt end.
#[js_type(export, no_deser)]
pub struct CopyPlannedItem {
	/// Index of the item (or entry) in the call.
	pub request: u64,
	pub source_uuid: Uuid,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
	pub is_dir: bool,
}

#[js_type(export, no_deser)]
pub struct CopiedTopLevelItem {
	/// Index of the item (or entry) in the call.
	pub request: u64,
	pub source_uuid: Uuid,
	pub item: NonRootNormalItemTagged,
}

#[js_type(export, no_deser)]
pub struct CopyFailure {
	/// The failed source, as it can be passed to `copyItemsTo` again.
	pub item: CopyItem,
	pub info: CopyFailureInfo,
}

/// The outcome of a copy, whether it completed, was cancelled or failed.
#[js_type(export, no_deser)]
pub struct CopyReport {
	/// Top-level items created, in creation order.
	pub top_level: Vec<CopiedTopLevelItem>,
	pub failures: Vec<CopyFailure>,
	pub skipped: Vec<CopySkippedEntry>,
	pub renamed: Vec<CopyRenamedEntry>,
	pub totals: PlanTotals,
	pub counts: CopyCounts,
	/// Why the copy ended early: kind `Cancelled` when cancelled, or the error that stopped it.
	/// `undefined` when it ran to the end, failures of single items included.
	pub error: Option<CopyError>,
}

impl TryFrom<CopyItem> for api::CopySource {
	type Error = Error;

	fn try_from(item: CopyItem) -> Result<Self, Error> {
		Ok(match item {
			CopyItem::File(file) => Self::File(RemoteFileType::try_from(file)?),
			CopyItem::Dir(dir) => Self::Dir(match DirByCategoryWithContext::from(dir) {
				DirByCategoryWithContext::Normal(DirType::Dir(dir)) => {
					api::CopySourceDir::Normal(dir.into_owned())
				}
				DirByCategoryWithContext::Normal(DirType::Root(_)) => {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						"the root directory cannot be copied",
					));
				}
				DirByCategoryWithContext::Shared(dir, role) => {
					api::CopySourceDir::Shared(dir, role)
				}
				DirByCategoryWithContext::Linked(dir, link) => {
					api::CopySourceDir::Linked(dir, link.try_into()?)
				}
			}),
		})
	}
}

impl From<api::FailedSource<api::CopySourceDir>> for CopyItem {
	fn from(source: api::FailedSource<api::CopySourceDir>) -> Self {
		match source {
			api::FailedSource::File(file) => Self::File(AnyFile::from(*file)),
			api::FailedSource::Dir(dir) => Self::Dir(match dir {
				api::CopySourceDir::Normal(dir) => {
					AnyDirWithContext::Normal(AnyNormalDir::Dir(dir.into()))
				}
				api::CopySourceDir::Shared(dir, role) => {
					AnyDirWithContext::Shared(AnySharedDirWithContext {
						dir: dir.into(),
						share_info: role,
					})
				}
				api::CopySourceDir::Linked(dir, link) => {
					AnyDirWithContext::Linked(AnyLinkedDirWithContext {
						dir: dir.into(),
						link: link.into(),
					})
				}
			}),
		}
	}
}

impl TryFrom<CopyEntry> for api::CopyRequest {
	type Error = Error;

	fn try_from(entry: CopyEntry) -> Result<Self, Error> {
		Ok(Self {
			source: entry.item.try_into()?,
			destination: DirType::from(entry.destination),
			name: entry.name,
		})
	}
}

/// The requests of `copyItems`: every item into the same destination.
fn requests_into(
	items: Vec<CopyItem>,
	destination: AnyNormalDir,
) -> Result<Vec<api::CopyRequest>, Error> {
	let destination = DirType::<'static, Normal>::from(destination);
	items
		.into_iter()
		.map(|item| {
			Ok(api::CopyRequest {
				source: item.try_into()?,
				destination: destination.clone(),
				name: None,
			})
		})
		.collect()
}

fn requests_to(entries: Vec<CopyEntry>) -> Result<Vec<api::CopyRequest>, Error> {
	entries.into_iter().map(TryFrom::try_from).collect()
}

impl From<&Error> for CopyError {
	fn from(error: &Error) -> Self {
		Self {
			kind: error.kind(),
			message: error.to_string(),
			server_message: error.server_message(),
			server_code: error.server_code(),
		}
	}
}

impl From<&api::FailureInfo> for CopyFailureInfo {
	fn from(info: &api::FailureInfo) -> Self {
		Self {
			source_uuid: info.source_uuid,
			source_path: info.source_path.clone(),
			dest_parent: info.dest_parent,
			dest_parent_dir: info.dest_parent_dir.clone().into(),
			dest_name: info.dest_name.clone(),
			stage: info.stage,
			error: CopyError::from(info.error.as_ref()),
			affected_files: info.affected_files,
			affected_bytes: info.affected_bytes,
			existing_file: info.existing_file,
		}
	}
}

impl From<&api::SkippedEntry> for CopySkippedEntry {
	fn from(entry: &api::SkippedEntry) -> Self {
		Self {
			source_path: entry.source_path.clone(),
			bytes: entry.bytes,
			reason: entry.reason,
		}
	}
}

impl From<&api::RenamedEntry> for CopyRenamedEntry {
	fn from(entry: &api::RenamedEntry) -> Self {
		Self {
			source_uuid: entry.source_uuid,
			source_path: entry.source_path.clone(),
			name: entry.name.as_ref().to_owned(),
			reason: entry.reason,
		}
	}
}

impl From<api::CopyEvent> for CopyEvent {
	fn from(event: api::CopyEvent) -> Self {
		match event {
			api::CopyEvent::DirCreated {
				source_uuid,
				dest_uuid,
				dest_parent,
				name,
			} => Self::DirCreated(CopyDirCreated {
				source_uuid,
				dest_uuid,
				dest_parent,
				name,
			}),
			api::CopyEvent::DirFailed(info) => Self::DirFailed((&info).into()),
			api::CopyEvent::FileStarted(file) => Self::FileStarted(file),
			api::CopyEvent::FileDone {
				source_uuid,
				dest_uuid,
				dest_parent,
				name,
				size,
			} => Self::FileDone(CopyFileDone {
				source_uuid,
				dest_uuid,
				dest_parent,
				name,
				size,
			}),
			api::CopyEvent::FileFailed(info) => Self::FileFailed((&info).into()),
			api::CopyEvent::Skipped {
				source_path,
				bytes,
				reason,
			} => Self::Skipped(CopySkippedEntry {
				source_path,
				bytes,
				reason,
			}),
			api::CopyEvent::Renamed {
				source_uuid,
				source_path,
				name,
				reason,
			} => Self::Renamed(CopyRenamedEntry {
				source_uuid,
				source_path,
				name,
				reason,
			}),
			api::CopyEvent::PropagationFailed { dest_uuid, error } => {
				Self::PropagationFailed(CopyItemError {
					dest_uuid,
					error: CopyError::from(error.as_ref()),
				})
			}
			api::CopyEvent::ColorFailed { dest_uuid, error } => Self::ColorFailed(CopyItemError {
				dest_uuid,
				error: CopyError::from(error.as_ref()),
			}),
		}
	}
}

fn millis(duration: Duration) -> u64 {
	u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl From<api::CopyUpdate> for CopyUpdate {
	fn from(update: api::CopyUpdate) -> Self {
		Self {
			phase: update.phase,
			run_state: update.run_state,
			scan: update.scan,
			totals: update.totals,
			counts: update.counts,
			active: update.active,
			events: update.events.into_iter().map(Into::into).collect(),
			bytes_per_second: update.bytes_per_second,
			eta_ms: update.eta.map(millis),
			active_time_ms: millis(update.active_time),
		}
	}
}

impl From<api::PlannedTopLevelItem> for CopyPlannedItem {
	fn from(item: api::PlannedTopLevelItem) -> Self {
		Self {
			request: item.request as u64,
			source_uuid: item.source_uuid,
			dest_uuid: item.dest_uuid,
			dest_parent: item.dest_parent,
			name: item.name,
			is_dir: item.is_dir,
		}
	}
}

impl From<api::CopiedTopLevel> for CopiedTopLevelItem {
	fn from(item: api::CopiedTopLevel) -> Self {
		Self {
			request: item.request as u64,
			source_uuid: item.source_uuid,
			item: item.item.into(),
		}
	}
}

impl From<api::CopyOutcome<api::CopySourceDir>> for CopyReport {
	fn from(outcome: api::CopyOutcome<api::CopySourceDir>) -> Self {
		let report = outcome.report;
		Self {
			top_level: report.top_level.into_iter().map(Into::into).collect(),
			failures: report
				.failures
				.into_iter()
				.map(|failure| CopyFailure {
					info: (&failure.info).into(),
					item: failure.source.into(),
				})
				.collect(),
			skipped: report.skipped.iter().map(Into::into).collect(),
			renamed: report.renamed.iter().map(Into::into).collect(),
			totals: report.totals,
			counts: report.counts,
			error: outcome.result.err().as_ref().map(CopyError::from),
		}
	}
}

/// A callback of the job, converted for the bindings.
enum Delivery {
	TopLevelPlanned(Vec<CopyPlannedItem>),
	TopLevelCreated(CopiedTopLevelItem),
	Update(CopyUpdate),
}

/// Passes the job's callbacks to the binding's delivery task over one channel, which keeps
/// their order.
struct DeliveryChannel(tokio::sync::mpsc::UnboundedSender<Delivery>);

impl api::CopyCallback for DeliveryChannel {
	fn top_level_planned(&self, items: Vec<api::PlannedTopLevelItem>) {
		let _ = self.0.send(Delivery::TopLevelPlanned(
			items.into_iter().map(Into::into).collect(),
		));
	}

	fn top_level_created(&self, item: api::CopiedTopLevel) {
		let _ = self.0.send(Delivery::TopLevelCreated(item.into()));
	}

	fn update(&self, update: api::CopyUpdate) {
		let _ = self.0.send(Delivery::Update(update.into()));
	}
}

/// Runs the copy as the job of `managed_future`, its callbacks going to `sender`.
fn copy_job(
	client: Arc<Client>,
	requests: Vec<api::CopyRequest>,
	max_bytes: Option<u64>,
	sender: tokio::sync::mpsc::UnboundedSender<Delivery>,
) -> impl FnOnce(api::JobControl) -> CopyJobFuture + Send + 'static {
	move |control| {
		Box::pin(async move {
			let outcome = client
				.copy_items_to(
					requests,
					api::CopyOptions { max_bytes },
					DeliveryChannel(sender),
					control,
				)
				.await;
			Ok(CopyReport::from(outcome))
		})
	}
}

type CopyJobFuture = crate::util::MaybeSendBoxFuture<'static, Result<CopyReport, Error>>;

#[cfg(feature = "uniffi")]
mod uniffi_impl {
	use std::sync::Arc;

	use crate::{Error, auth::JsClient, js::AnyNormalDir, js::ManagedFuture};

	use super::{
		CopiedTopLevelItem, CopyEntry, CopyItem, CopyPlannedItem, CopyReport, CopyUpdate, Delivery,
		copy_job, requests_into, requests_to,
	};

	/// Receives a copy's progress, in the order the copy made it, before the call returns.
	#[uniffi::export(with_foreign)]
	pub trait CopyItemsCallback: Send + Sync {
		/// The top-level items, before any of them is created.
		fn on_top_level_planned(&self, items: Vec<CopyPlannedItem>);
		/// A top-level item was created.
		fn on_top_level_created(&self, item: CopiedTopLevelItem);
		fn on_update(&self, update: CopyUpdate);
	}

	#[derive(uniffi::Record, Default)]
	pub struct CopyItemsOptions {
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written.
		#[uniffi(default = None)]
		pub max_bytes: Option<u64>,
	}

	/// Delivers every callback in order; the foreign callbacks may block, so this runs on a
	/// blocking thread. Returns once the job has dropped its sender and all is delivered.
	pub(super) fn deliver(
		mut receiver: tokio::sync::mpsc::UnboundedReceiver<Delivery>,
		callback: &dyn CopyItemsCallback,
	) {
		while let Some(delivery) = receiver.blocking_recv() {
			match delivery {
				Delivery::TopLevelPlanned(items) => callback.on_top_level_planned(items),
				Delivery::TopLevelCreated(item) => callback.on_top_level_created(item),
				Delivery::Update(update) => callback.on_update(update),
			}
		}
	}

	async fn run(
		client: Arc<crate::auth::Client>,
		requests: Vec<crate::fs::copy::CopyRequest>,
		options: CopyItemsOptions,
		callback: Arc<dyn CopyItemsCallback>,
		managed_future: ManagedFuture,
	) -> Result<CopyReport, Error> {
		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
		let job = copy_job(client, requests, options.max_bytes, sender);
		managed_future
			.into_js_managed_commander_job(move |control| async move {
				let delivery =
					tokio::task::spawn_blocking(move || deliver(receiver, callback.as_ref()));
				let result = job(control).await;
				// the job has ended and dropped its sender: this returns once everything it
				// reported was delivered
				let _ = delivery.await;
				result
			})
			.await
	}

	#[uniffi::export]
	impl JsClient {
		/// Copies `items` into `destination`. There is no server-side copy: every file is
		/// downloaded and uploaded again, and every directory is created anew. A name taken at
		/// the destination is kept, and the copy is named `name (1)`, `name (2)`, ...
		///
		/// Pause and abort through `managed_future` reach the whole copy. The report is returned
		/// whether the copy completed, was cancelled or failed; its `failures` can be passed
		/// to `copy_items_to` to try them again.
		pub async fn copy_items(
			&self,
			items: Vec<CopyItem>,
			destination: AnyNormalDir,
			options: CopyItemsOptions,
			callback: Arc<dyn CopyItemsCallback>,
			managed_future: ManagedFuture,
		) -> Result<CopyReport, Error> {
			let requests = requests_into(items, destination)?;
			run(self.inner(), requests, options, callback, managed_future).await
		}

		/// [`copy_items`](Self::copy_items), with a destination (and optionally a name) for
		/// each entry.
		pub async fn copy_items_to(
			&self,
			entries: Vec<CopyEntry>,
			options: CopyItemsOptions,
			callback: Arc<dyn CopyItemsCallback>,
			managed_future: ManagedFuture,
		) -> Result<CopyReport, Error> {
			let requests = requests_to(entries)?;
			run(self.inner(), requests, options, callback, managed_future).await
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"))]
mod wasm_impl {
	use std::sync::Arc;

	use filen_macros::js_type;
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	use crate::{
		Error,
		auth::JsClient,
		js::{AnyLinkedDirTagged, AnyNormalDir, AnySharedDirTagged, ManagedFuture},
	};

	use super::{
		AnyDirWithContext, AnyFile, CopyEntry, CopyItem, CopyReport, Delivery, copy_job,
		requests_into, requests_to,
	};

	#[js_type(import, no_ser, no_default)]
	pub struct CopyItemsParams {
		pub items: Vec<CopyItem>,
		pub destination: AnyNormalDir,
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written.
		#[serde(default)]
		#[tsify(optional)]
		pub max_bytes: Option<u64>,
		#[tsify(type = "(update: CopyUpdate) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_update: js_sys::Function,
		/// The top-level items, before any of them is created.
		#[tsify(type = "(items: CopyPlannedItem[]) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_top_level_planned: js_sys::Function,
		#[tsify(type = "(item: CopiedTopLevelItem) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_top_level_created: js_sys::Function,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	#[js_type(import, no_ser, no_default)]
	pub struct CopyItemsToParams {
		pub entries: Vec<CopyEntry>,
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written.
		#[serde(default)]
		#[tsify(optional)]
		pub max_bytes: Option<u64>,
		#[tsify(type = "(update: CopyUpdate) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_update: js_sys::Function,
		/// The top-level items, before any of them is created.
		#[tsify(type = "(items: CopyPlannedItem[]) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_top_level_planned: js_sys::Function,
		#[tsify(type = "(item: CopiedTopLevelItem) => void", optional)]
		#[serde(default, with = "serde_wasm_bindgen::preserve")]
		pub on_top_level_created: js_sys::Function,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	struct Callbacks {
		on_update: js_sys::Function,
		on_top_level_planned: js_sys::Function,
		on_top_level_created: js_sys::Function,
	}

	impl Callbacks {
		fn deliver(&self, delivery: Delivery) {
			let serializer = serde_wasm_bindgen::Serializer::new()
				.serialize_maps_as_objects(true)
				.serialize_large_number_types_as_bigints(true);
			let (callback, value) = match &delivery {
				Delivery::TopLevelPlanned(items) => (
					&self.on_top_level_planned,
					serde::Serialize::serialize(items, &serializer),
				),
				Delivery::TopLevelCreated(item) => (
					&self.on_top_level_created,
					serde::Serialize::serialize(item, &serializer),
				),
				Delivery::Update(update) => (
					&self.on_update,
					serde::Serialize::serialize(update, &serializer),
				),
			};
			if callback.is_undefined() {
				return;
			}
			match value {
				Ok(value) => {
					let _ = callback.call1(&JsValue::UNDEFINED, &value);
				}
				Err(error) => tracing::error!("failed to convert a copy callback: {error}"),
			}
		}
	}

	async fn run(
		client: Arc<crate::auth::Client>,
		requests: Vec<crate::fs::copy::CopyRequest>,
		max_bytes: Option<u64>,
		callbacks: Callbacks,
		managed_future: ManagedFuture,
	) -> Result<CopyReport, Error> {
		// The JS functions never leave this thread: the job sends its callbacks over a channel,
		// and this task calls them here, in order.
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
		let (drained, drained_receiver) = tokio::sync::oneshot::channel::<()>();
		crate::runtime::spawn_local(async move {
			while let Some(delivery) = receiver.recv().await {
				callbacks.deliver(delivery);
			}
			let _ = drained.send(());
		});
		let job = copy_job(client, requests, max_bytes, sender);
		let result = match managed_future.into_js_managed_commander_job(job) {
			Ok(running) => running.await,
			Err(error) => Err(error),
		};
		// the job has ended and dropped its sender: everything it reported reaches its
		// callback before the result does
		let _ = drained_receiver.await;
		result
	}

	#[wasm_bindgen(js_class = "Client")]
	impl JsClient {
		/// Copies `items` into `destination`. There is no server-side copy: every file is
		/// downloaded and uploaded again, and every directory is created anew. A name taken at
		/// the destination is kept, and the copy is named `name (1)`, `name (2)`, ...
		///
		/// Pause and abort through `managedFuture` reach the whole copy. The report is returned
		/// whether the copy completed, was cancelled or failed; its `failures` can be passed
		/// to `copyItemsTo` to try them again.
		#[wasm_bindgen(js_name = "copyItems")]
		pub async fn copy_items(&self, params: CopyItemsParams) -> Result<CopyReport, Error> {
			let requests = requests_into(params.items, params.destination)?;
			let callbacks = Callbacks {
				on_update: params.on_update,
				on_top_level_planned: params.on_top_level_planned,
				on_top_level_created: params.on_top_level_created,
			};
			run(
				self.inner(),
				requests,
				params.max_bytes,
				callbacks,
				params.managed_future,
			)
			.await
		}

		/// `copyItems`, with a destination (and optionally a name) for each entry.
		#[wasm_bindgen(js_name = "copyItemsTo")]
		pub async fn copy_items_to(&self, params: CopyItemsToParams) -> Result<CopyReport, Error> {
			let requests = requests_to(params.entries)?;
			let callbacks = Callbacks {
				on_update: params.on_update,
				on_top_level_planned: params.on_top_level_planned,
				on_top_level_created: params.on_top_level_created,
			};
			run(
				self.inner(),
				requests,
				params.max_bytes,
				callbacks,
				params.managed_future,
			)
			.await
		}
	}

	/// The shape `CopyItem` is read from, so a failure's item can be passed back as it is.
	impl serde::Serialize for CopyItem {
		fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
			use serde::ser::SerializeStruct;
			match self {
				Self::File(AnyFile::File(file)) => file.serialize(serializer),
				Self::File(AnyFile::Shared(file)) => file.serialize(serializer),
				Self::File(AnyFile::Linked(file)) => file.serialize(serializer),
				Self::Dir(AnyDirWithContext::Normal(dir)) => dir.serialize(serializer),
				Self::Dir(AnyDirWithContext::Shared(shared)) => {
					let mut state = serializer.serialize_struct("AnySharedDirWithContext", 2)?;
					state.serialize_field("dir", &AnySharedDirTagged::from(shared.dir.clone()))?;
					state.serialize_field("shareInfo", &shared.share_info)?;
					state.end()
				}
				Self::Dir(AnyDirWithContext::Linked(linked)) => {
					let mut state = serializer.serialize_struct("AnyLinkedDirWithContext", 2)?;
					state.serialize_field("dir", &AnyLinkedDirTagged::from(linked.dir.clone()))?;
					state.serialize_field("link", &linked.link)?;
					state.end()
				}
			}
		}
	}
}

#[cfg(all(test, feature = "uniffi"))]
mod tests {
	use std::{borrow::Cow, sync::Mutex};

	use chrono::Utc;
	use filen_types::{api::v3::dir::color::DirColor, fs::ParentUuid};

	use super::{uniffi_impl::CopyItemsCallback, *};
	use crate::{
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{HasUUID, file::traits::HasFileInfo},
		fs::{
			dir::{
				RemoteDirectory, RootDirectory,
				meta::{DecryptedDirectoryMeta, DirectoryMeta},
			},
			file::{
				RemoteFile,
				meta::{DecryptedFileMeta, FileMeta},
			},
		},
		js::Root,
	};

	fn file() -> RemoteFileType<'static> {
		let file: crate::fs::file::AnonymousRemoteFile = RemoteFile::from_meta(
			Uuid::new_v4(),
			(),
			Uuid::new_v4().into(),
			10,
			1,
			"de-1",
			"bucket",
			Utc::now(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Borrowed("a.txt"),
				size: 10,
				mime: Cow::Borrowed("text/plain"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: Utc::now(),
				created: None,
				hash: None,
			}),
		);
		RemoteFileType::File(Cow::Owned(file))
	}

	fn dir() -> RemoteDirectory {
		RemoteDirectory::from_meta(
			Uuid::new_v4(),
			ParentUuid::Uuid(Uuid::new_v4()),
			DirColor::Blue,
			false,
			Utc::now(),
			DirectoryMeta::Decoded(DecryptedDirectoryMeta {
				name: Cow::Borrowed("Photos"),
				created: None,
			}),
		)
	}

	fn failure_source(item: CopyItem) -> api::CopySource {
		api::CopySource::try_from(item).expect("a failed item is a copy source again")
	}

	#[test]
	fn a_failed_file_is_a_copy_source_again() {
		let source = file();
		let item = CopyItem::from(api::FailedSource::File(Box::new(source.clone())));
		let api::CopySource::File(copied) = failure_source(item) else {
			panic!("a file");
		};
		assert_eq!(copied.uuid(), source.uuid());
		assert_eq!(copied.size(), source.size());
	}

	#[test]
	fn a_failed_directory_is_a_copy_source_again() {
		let source = dir();
		let item = CopyItem::from(api::FailedSource::Dir(api::CopySourceDir::Normal(
			source.clone(),
		)));
		let api::CopySource::Dir(api::CopySourceDir::Normal(copied)) = failure_source(item) else {
			panic!("a directory of the user's drive");
		};
		assert_eq!(copied.uuid(), source.uuid());
	}

	#[test]
	fn a_failed_shared_or_linked_directory_is_a_copy_source_again() {
		use crate::{
			auth::MetaKey,
			connect::{
				DirPublicLink, PasswordState,
				fs::{ShareInfo, SharedDirectory, SharingRole},
			},
			fs::dir::LinkedDirectory,
		};
		use filen_types::api::v3::dir::link::info::LinkPasswordSalt;

		let shared = dir();
		let role = SharingRole::Receiver(ShareInfo {
			email: "sharer@example.com".to_owned(),
			id: 7,
		});
		let item = CopyItem::from(api::FailedSource::Dir(api::CopySourceDir::Shared(
			DirType::Dir(Cow::Owned(SharedDirectory {
				inner: shared.clone(),
			})),
			role.clone(),
		)));
		let api::CopySource::Dir(api::CopySourceDir::Shared(dir_back, role_back)) =
			failure_source(item)
		else {
			panic!("a shared directory");
		};
		assert_eq!(dir_back.uuid(), shared.uuid());
		assert_eq!(role_back, role);

		let linked = dir();
		let link = DirPublicLink {
			link_uuid: Uuid::new_v4(),
			link_key: MetaKey::V3(EncryptionKey::generate()),
			password: PasswordState::None,
			enable_download: true,
			salt: LinkPasswordSalt::None,
		};
		let item = CopyItem::from(api::FailedSource::Dir(api::CopySourceDir::Linked(
			DirType::Dir(Cow::Owned(LinkedDirectory(linked.clone()))),
			link.clone(),
		)));
		let api::CopySource::Dir(api::CopySourceDir::Linked(dir_back, link_back)) =
			failure_source(item)
		else {
			panic!("a directory in a public link");
		};
		assert_eq!(dir_back.uuid(), linked.uuid());
		assert_eq!(link_back, link, "the link, with its key, comes back intact");
	}

	#[test]
	fn the_root_directory_cannot_be_copied() {
		let root = CopyItem::Dir(AnyDirWithContext::Normal(AnyNormalDir::Root(Root::from(
			RootDirectory::new(Uuid::new_v4()),
		))));
		let error = api::CopySource::try_from(root).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState);
	}

	#[test]
	fn an_update_reports_milliseconds_and_the_parts_of_its_errors() {
		let parent = dir();
		let failure = api::FailureInfo {
			source_uuid: Uuid::new_v4(),
			source_path: "/a.txt".to_owned(),
			dest_parent: parent.uuid(),
			dest_parent_dir: DirType::Dir(Cow::Owned(parent.clone())),
			dest_name: "a.txt".to_owned(),
			stage: CopyStage::Upload,
			error: Arc::new(Error::custom(ErrorKind::MaxStorageReached, "full")),
			affected_files: 1,
			affected_bytes: 10,
			existing_file: None,
		};
		let update = CopyUpdate::from(api::CopyUpdate {
			phase: CopyPhase::CopyingFiles,
			run_state: RunState::Running,
			scan: ScanProgress::default(),
			totals: PlanTotals::default(),
			counts: CopyCounts::default(),
			active: Vec::new(),
			events: vec![api::CopyEvent::FileFailed(failure.clone())],
			bytes_per_second: Some(100),
			eta: Some(Duration::from_millis(1500)),
			active_time: Duration::from_secs_f64(2.5),
		});
		assert_eq!(update.eta_ms, Some(1500));
		assert_eq!(update.active_time_ms, 2500);
		let [CopyEvent::FileFailed(info)] = update.events.as_slice() else {
			panic!("one failed file");
		};
		assert_eq!(info.error.kind, ErrorKind::MaxStorageReached);
		let AnyNormalDir::Dir(parent_dir) = &info.dest_parent_dir else {
			panic!("the directory the file was to be created in");
		};
		assert_eq!(
			DirType::<'static, Normal>::from(AnyNormalDir::Dir(parent_dir.clone())).uuid(),
			parent.uuid(),
			"a retry can target the parent without looking it up"
		);
		assert_eq!(info.source_uuid, failure.source_uuid);
		assert_eq!(info.affected_bytes, 10);
	}

	#[test]
	fn a_report_carries_why_the_copy_ended() {
		let cancelled = CopyReport::from(api::CopyOutcome::<api::CopySourceDir> {
			report: api::CopyReport::default(),
			result: Err(Error::custom(ErrorKind::Cancelled, "copy cancelled")),
		});
		assert_eq!(cancelled.error.map(|e| e.kind), Some(ErrorKind::Cancelled));
		let done = CopyReport::from(api::CopyOutcome::<api::CopySourceDir> {
			report: api::CopyReport::default(),
			result: Ok(()),
		});
		assert!(done.error.is_none());
	}

	#[derive(Default)]
	struct Recorder(Mutex<Vec<u64>>);

	impl CopyItemsCallback for Recorder {
		fn on_top_level_planned(&self, items: Vec<CopyPlannedItem>) {
			self.0
				.lock()
				.unwrap()
				.extend(items.iter().map(|i| i.request));
		}

		fn on_top_level_created(&self, item: CopiedTopLevelItem) {
			self.0.lock().unwrap().push(item.request);
		}

		fn on_update(&self, update: CopyUpdate) {
			self.0.lock().unwrap().push(update.active_time_ms);
		}
	}

	fn planned(request: u64) -> api::PlannedTopLevelItem {
		api::PlannedTopLevelItem {
			request: request as usize,
			source_uuid: Uuid::new_v4(),
			dest_uuid: Uuid::new_v4(),
			dest_parent: Uuid::new_v4(),
			name: "a".to_owned(),
			is_dir: false,
		}
	}

	fn update(millis: u64) -> api::CopyUpdate {
		api::CopyUpdate {
			phase: CopyPhase::CopyingFiles,
			run_state: RunState::Running,
			scan: ScanProgress::default(),
			totals: PlanTotals::default(),
			counts: CopyCounts::default(),
			active: Vec::new(),
			events: Vec::new(),
			bytes_per_second: None,
			eta: None,
			active_time: Duration::from_millis(millis),
		}
	}

	#[test]
	fn callbacks_are_delivered_in_order_until_the_job_lets_go() {
		use api::CopyCallback;

		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
		let recorder = Arc::new(Recorder::default());
		let delivering = {
			let recorder = Arc::clone(&recorder);
			std::thread::spawn(move || uniffi_impl::deliver(receiver, recorder.as_ref()))
		};
		let channel = DeliveryChannel(sender);
		let mut expected = Vec::new();
		for i in 0..300 {
			match i % 3 {
				0 => channel.top_level_planned(vec![planned(i)]),
				1 => channel.update(update(i)),
				_ => channel.top_level_created(api::CopiedTopLevel {
					request: i as usize,
					source_uuid: Uuid::new_v4(),
					item: crate::fs::categories::NonRootItemType::Dir(Cow::Owned(dir())),
				}),
			}
			expected.push(i);
		}
		drop(channel);
		delivering
			.join()
			.expect("delivery ends once the job drops its sender");
		assert_eq!(*recorder.0.lock().unwrap(), expected);
	}
}
