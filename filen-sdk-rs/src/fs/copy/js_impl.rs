//! `copyItems` / `copyItemsTo` for the wasm and uniffi bindings. Both platforms take the same
//! items and report the same types; the job's callbacks reach the caller in the order the job
//! made them, all before the call returns.

use std::sync::Arc;

use filen_macros::js_type;
use filen_types::fs::Uuid;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		file::enums::RemoteFileType,
		name::ValidatedName,
	},
	job::{ItemError, JobError, job_error, millis},
	js::{
		AnyDirWithContext, AnyFile, AnyItemWithContext, AnyLinkedDirWithContext, AnyNormalDir,
		AnySharedDirWithContext, DirByCategoryWithContext, NonRootNormalItemTagged,
	},
};

// The core types this module mirrors under the same name (CopyEvent, CopyUpdate, CopyReport,
// CopyFailure) are written out as `super::X`.
use super::{
	ActiveFile, CopiedTopLevel, CopyCallback, CopyConfig, CopyFailed, CopyPhase, CopyRequest,
	CopyStage, FailedSource, FailureInfo, ItemCounts, ItemSource, ItemSourceDir, JobControl,
	PlanTotals, PlannedTopLevelItem, RenamedEntry, RunState, ScanProgress, SkippedEntry,
};

/// One item to copy into its own destination, optionally under another name.
#[js_type(import, no_ser)]
pub struct CopyEntry {
	pub item: AnyItemWithContext,
	pub destination: AnyNormalDir,
	#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub name: Option<String>,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CopyFailureInfo {
	pub source_uuid: Uuid,
	pub source_path: String,
	/// The directory the item was to be created in.
	pub dest_parent: Uuid,
	/// The same directory, to retry the item in with `copyItemsTo`.
	pub dest_parent_dir: AnyNormalDir,
	pub dest_name: String,
	pub stage: CopyStage,
	pub error: JobError,
	/// Files and bytes not copied because of this failure (a directory's whole subtree).
	pub affected_files: u64,
	pub affected_bytes: u64,
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

#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum CopyEvent {
	DirCreated(CopyDirCreated),
	DirFailed(CopyFailureInfo),
	FileStarted(ActiveFile),
	FileDone(CopyFileDone),
	FileFailed(CopyFailureInfo),
	Skipped(SkippedEntry),
	Renamed(RenamedEntry),
	PropagationFailed(ItemError),
	ColorFailed(ItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CopyUpdate {
	pub phase: CopyPhase,
	pub run_state: RunState,
	pub scan: ScanProgress,
	pub totals: PlanTotals,
	pub counts: ItemCounts,
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

#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CopyFailure {
	/// The failed source, as it can be passed to `copyItemsTo` again.
	pub item: AnyItemWithContext,
	pub info: CopyFailureInfo,
}

/// The outcome of a copy, whether it completed, was cancelled or failed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CopyReport {
	/// Top-level items created, in creation order.
	pub top_level: Vec<CopiedTopLevelItem>,
	pub failures: Vec<CopyFailure>,
	pub skipped: Vec<SkippedEntry>,
	pub renamed: Vec<RenamedEntry>,
	pub totals: PlanTotals,
	pub counts: ItemCounts,
	/// Why the copy ended early: kind `Cancelled` when cancelled, or the error that stopped it.
	/// `undefined` when it ran to the end, failures of single items included.
	pub error: Option<JobError>,
}

impl TryFrom<AnyItemWithContext> for ItemSource {
	type Error = Error;

	fn try_from(item: AnyItemWithContext) -> Result<Self, Error> {
		Ok(match item {
			AnyItemWithContext::File(file) => Self::File(RemoteFileType::try_from(file)?),
			AnyItemWithContext::Dir(dir) => Self::Dir(match DirByCategoryWithContext::from(dir) {
				DirByCategoryWithContext::Normal(DirType::Dir(dir)) => {
					ItemSourceDir::Normal(dir.into_owned())
				}
				DirByCategoryWithContext::Normal(DirType::Root(_)) => {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						"the root directory cannot be copied",
					));
				}
				DirByCategoryWithContext::Shared(dir, role) => ItemSourceDir::Shared(dir, role),
				DirByCategoryWithContext::Linked(dir, link) => {
					ItemSourceDir::Linked(dir, link.try_into()?)
				}
			}),
		})
	}
}

impl From<FailedSource> for AnyItemWithContext {
	fn from(source: FailedSource) -> Self {
		match source {
			FailedSource::File(file) => Self::File(AnyFile::from(*file)),
			FailedSource::Dir(dir) => Self::Dir(match dir {
				ItemSourceDir::Normal(dir) => {
					AnyDirWithContext::Normal(AnyNormalDir::Dir(dir.into()))
				}
				ItemSourceDir::Shared(dir, role) => {
					AnyDirWithContext::Shared(AnySharedDirWithContext {
						dir: dir.into(),
						share_info: role,
					})
				}
				ItemSourceDir::Linked(dir, link) => {
					AnyDirWithContext::Linked(AnyLinkedDirWithContext {
						dir: dir.into(),
						link: link.into(),
					})
				}
			}),
		}
	}
}

// CopyEntry carries the name unvalidated because neither tsify's from_wasm_abi nor uniffi's
// record lifting can report an error; an invalid name fails the call here, before the copy starts.
impl TryFrom<CopyEntry> for CopyRequest {
	type Error = Error;

	fn try_from(entry: CopyEntry) -> Result<Self, Error> {
		Ok(Self {
			source: entry.item.try_into()?,
			destination: DirType::from(entry.destination),
			name: entry
				.name
				.as_deref()
				.map(ValidatedName::try_from)
				.transpose()?,
		})
	}
}

/// The requests of `copyItems`: every item into the same destination.
fn requests_into(
	items: Vec<AnyItemWithContext>,
	destination: AnyNormalDir,
) -> Result<Vec<CopyRequest>, Error> {
	let destination = DirType::<'static, Normal>::from(destination);
	items
		.into_iter()
		.map(|item| {
			Ok(CopyRequest {
				source: item.try_into()?,
				destination: destination.clone(),
				name: None,
			})
		})
		.collect()
}

fn requests_to(entries: Vec<CopyEntry>) -> Result<Vec<CopyRequest>, Error> {
	entries.into_iter().map(TryFrom::try_from).collect()
}

impl From<FailureInfo> for CopyFailureInfo {
	fn from(info: FailureInfo) -> Self {
		Self {
			source_uuid: info.source_uuid,
			source_path: info.source_path,
			dest_parent: info.dest_parent_dir.uuid(),
			dest_parent_dir: info.dest_parent_dir.into(),
			dest_name: info.dest_name,
			stage: info.stage,
			error: job_error(info.error),
			affected_files: info.affected_files,
			affected_bytes: info.affected_bytes,
		}
	}
}

impl From<super::CopyFailure> for CopyFailure {
	fn from(failure: super::CopyFailure) -> Self {
		Self {
			item: failure.source.into(),
			info: failure.info.into(),
		}
	}
}

impl From<super::CopyEvent> for CopyEvent {
	fn from(event: super::CopyEvent) -> Self {
		match event {
			super::CopyEvent::DirCreated {
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
			super::CopyEvent::DirFailed(info) => Self::DirFailed(info.into()),
			super::CopyEvent::FileStarted(file) => Self::FileStarted(file),
			super::CopyEvent::FileDone {
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
			super::CopyEvent::FileFailed(info) => Self::FileFailed(info.into()),
			super::CopyEvent::Skipped(entry) => Self::Skipped(entry),
			super::CopyEvent::Renamed(entry) => Self::Renamed(entry),
			super::CopyEvent::PropagationFailed { dest_uuid, error } => {
				Self::PropagationFailed(ItemError::new(dest_uuid, error))
			}
			super::CopyEvent::ColorFailed { dest_uuid, error } => {
				Self::ColorFailed(ItemError::new(dest_uuid, error))
			}
		}
	}
}

impl From<super::CopyUpdate> for CopyUpdate {
	fn from(update: super::CopyUpdate) -> Self {
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

impl From<PlannedTopLevelItem> for CopyPlannedItem {
	fn from(item: PlannedTopLevelItem) -> Self {
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

impl From<CopiedTopLevel> for CopiedTopLevelItem {
	fn from(item: CopiedTopLevel) -> Self {
		Self {
			request: item.request as u64,
			source_uuid: item.source_uuid,
			item: item.item.into(),
		}
	}
}

impl From<super::CopyReport> for CopyReport {
	fn from(report: super::CopyReport) -> Self {
		Self {
			top_level: report.top_level.into_iter().map(Into::into).collect(),
			failures: report.failures.into_iter().map(Into::into).collect(),
			skipped: report.skipped,
			renamed: report.renamed,
			totals: report.totals,
			counts: report.counts,
			error: None,
		}
	}
}

impl From<CopyFailed> for CopyReport {
	fn from(failed: CopyFailed) -> Self {
		Self {
			error: Some(job_error(failed.error)),
			..failed.report.into()
		}
	}
}

/// A copy's callback, converted for the bindings.
enum CopyDelivery {
	TopLevelPlanned(Vec<CopyPlannedItem>),
	TopLevelCreated(CopiedTopLevelItem),
	Update(CopyUpdate),
}

/// Passes a copy's callbacks to the binding's delivery task over one channel, which keeps
/// their order.
struct CopyChannel(UnboundedSender<CopyDelivery>);

impl CopyCallback for CopyChannel {
	fn on_top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		let _ = self.0.send(CopyDelivery::TopLevelPlanned(
			items.into_iter().map(Into::into).collect(),
		));
	}

	fn on_top_level_created(&self, item: CopiedTopLevel) {
		let _ = self.0.send(CopyDelivery::TopLevelCreated(item.into()));
	}

	fn on_update(&self, update: super::CopyUpdate) {
		let _ = self.0.send(CopyDelivery::Update(update.into()));
	}
}

/// Runs the copy as the job of a managed future, its callbacks going to `sender`.
async fn copy_job(
	client: Arc<Client>,
	requests: Vec<CopyRequest>,
	max_bytes: Option<u64>,
	sender: UnboundedSender<CopyDelivery>,
	control: JobControl,
) -> Result<CopyReport, Error> {
	let result = client
		.copy_items_to(
			requests,
			CopyConfig { max_bytes },
			CopyChannel(sender),
			control,
		)
		.await;
	// a copy that ended early still resolves, with the report of what it did
	Ok(match result {
		Ok(report) => report.into(),
		Err(failed) => failed.into(),
	})
}

#[cfg(feature = "uniffi")]
mod uniffi_impl {
	use std::sync::Arc;

	use crate::{
		Error,
		auth::JsClient,
		js::{AnyItemWithContext, AnyNormalDir, ManagedFuture},
	};

	use super::{
		Client, CopiedTopLevelItem, CopyDelivery, CopyEntry, CopyPlannedItem, CopyReport,
		CopyRequest, CopyUpdate, copy_job, requests_into, requests_to,
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
	pub struct CopyItemsConfig {
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written. Its report still carries `totals`, counted as not attempted, so the caller
		/// can tell how much storage the copy needs.
		#[uniffi(default = None)]
		pub max_bytes: Option<u64>,
	}

	pub(super) fn deliver_copy(callback: &dyn CopyItemsCallback, delivery: CopyDelivery) {
		match delivery {
			CopyDelivery::TopLevelPlanned(items) => callback.on_top_level_planned(items),
			CopyDelivery::TopLevelCreated(item) => callback.on_top_level_created(item),
			CopyDelivery::Update(update) => callback.on_update(update),
		}
	}

	async fn run(
		client: Arc<Client>,
		requests: Vec<CopyRequest>,
		config: CopyItemsConfig,
		callback: Arc<dyn CopyItemsCallback>,
		managed_future: ManagedFuture,
	) -> Result<CopyReport, Error> {
		managed_future
			.into_ordered_job(
				move |delivery| deliver_copy(callback.as_ref(), delivery),
				move |sender, control| {
					copy_job(client, requests, config.max_bytes, sender, control)
				},
			)
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
			items: Vec<AnyItemWithContext>,
			destination: AnyNormalDir,
			config: CopyItemsConfig,
			callback: Arc<dyn CopyItemsCallback>,
			managed_future: ManagedFuture,
		) -> Result<CopyReport, Error> {
			let requests = requests_into(items, destination)?;
			run(self.inner(), requests, config, callback, managed_future).await
		}

		/// [`copy_items`](Self::copy_items), with a destination (and optionally a name) for
		/// each entry.
		pub async fn copy_items_to(
			&self,
			entries: Vec<CopyEntry>,
			config: CopyItemsConfig,
			callback: Arc<dyn CopyItemsCallback>,
			managed_future: ManagedFuture,
		) -> Result<CopyReport, Error> {
			let requests = requests_to(entries)?;
			run(self.inner(), requests, config, callback, managed_future).await
		}
	}
}

#[cfg(feature = "wasm-full")]
mod wasm_impl {
	use std::sync::Arc;

	use filen_macros::js_type;
	use wasm_bindgen::prelude::wasm_bindgen;
	use web_sys::js_sys;

	use crate::{
		Error,
		auth::JsClient,
		js::{AnyItemWithContext, AnyNormalDir, ManagedFuture, call_callback},
	};

	use super::{
		Client, CopyDelivery, CopyEntry, CopyReport, CopyRequest, copy_job, requests_into,
		requests_to,
	};

	#[js_type(import, no_ser, no_default)]
	pub struct CopyItemsParams {
		pub items: Vec<AnyItemWithContext>,
		pub destination: AnyNormalDir,
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written. Its report still carries `totals`, counted as not attempted, so the caller
		/// can tell how much storage the copy needs.
		#[serde(default)]
		#[tsify(type = "number | bigint", optional)]
		pub max_bytes: Option<u64>,
		#[tsify(type = "(update: CopyUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The top-level items, before any of them is created.
		#[tsify(type = "(items: CopyPlannedItem[]) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_top_level_planned: Option<js_sys::Function>,
		#[tsify(type = "(item: CopiedTopLevelItem) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_top_level_created: Option<js_sys::Function>,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	#[js_type(import, no_ser, no_default)]
	pub struct CopyItemsToParams {
		pub entries: Vec<CopyEntry>,
		/// Storage still free on the account, if known: a larger copy fails before anything is
		/// written. Its report still carries `totals`, counted as not attempted, so the caller
		/// can tell how much storage the copy needs.
		#[serde(default)]
		#[tsify(type = "number | bigint", optional)]
		pub max_bytes: Option<u64>,
		#[tsify(type = "(update: CopyUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The top-level items, before any of them is created.
		#[tsify(type = "(items: CopyPlannedItem[]) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_top_level_planned: Option<js_sys::Function>,
		#[tsify(type = "(item: CopiedTopLevelItem) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_top_level_created: Option<js_sys::Function>,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	struct CopyCallbacks {
		on_update: Option<js_sys::Function>,
		on_top_level_planned: Option<js_sys::Function>,
		on_top_level_created: Option<js_sys::Function>,
	}

	impl CopyCallbacks {
		fn deliver(&self, delivery: CopyDelivery) {
			match delivery {
				CopyDelivery::TopLevelPlanned(items) => {
					call_callback(self.on_top_level_planned.as_ref(), &items)
				}
				CopyDelivery::TopLevelCreated(item) => {
					call_callback(self.on_top_level_created.as_ref(), &item)
				}
				CopyDelivery::Update(update) => call_callback(self.on_update.as_ref(), &update),
			}
		}
	}

	async fn run(
		client: Arc<Client>,
		requests: Vec<CopyRequest>,
		max_bytes: Option<u64>,
		callbacks: CopyCallbacks,
		managed_future: ManagedFuture,
	) -> Result<CopyReport, Error> {
		managed_future
			.into_ordered_job(
				move |delivery| callbacks.deliver(delivery),
				move |sender, control| copy_job(client, requests, max_bytes, sender, control),
			)
			.await
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
			let callbacks = CopyCallbacks {
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
			let callbacks = CopyCallbacks {
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
}

#[cfg(all(test, feature = "uniffi"))]
mod tests {
	use std::{borrow::Cow, time::Duration};

	use filen_types::api::v3::dir::link::info::LinkPasswordSalt;

	use super::{uniffi_impl::CopyItemsCallback, *};
	use crate::{
		auth::MetaKey,
		connect::{
			DirPublicLink, PasswordState,
			fs::{ShareInfo, SharedDirectory, SharingRole},
		},
		crypto::{shared::CreateRandom, v3::EncryptionKey},
		fs::{
			archive::test_support::remote_file,
			categories::NonRootItemType,
			copy,
			dir::{LinkedDirectory, RootDirectory},
			file::traits::HasFileInfo,
		},
		js::{
			Root,
			test_support::{PARENT, Recorder, delivered_in_order, drive_dir},
		},
	};

	fn file() -> RemoteFileType<'static> {
		remote_file(Uuid::from_u128(0xf), PARENT, "a.txt", b"ten bytes!", None)
	}

	fn failure_source(item: AnyItemWithContext) -> ItemSource {
		ItemSource::try_from(item).expect("a failed item is a copy source again")
	}

	#[test]
	fn a_failed_file_is_a_copy_source_again() {
		let source = file();
		let item = AnyItemWithContext::from(FailedSource::File(Box::new(source.clone())));
		let ItemSource::File(copied) = failure_source(item) else {
			panic!("a file");
		};
		assert_eq!(copied.uuid(), source.uuid());
		assert_eq!(copied.size(), source.size());
	}

	#[test]
	fn a_failed_directory_is_a_copy_source_again() {
		let source = drive_dir(Uuid::from_u128(0xd), "Photos");
		let item =
			AnyItemWithContext::from(FailedSource::Dir(ItemSourceDir::Normal(source.clone())));
		let ItemSource::Dir(ItemSourceDir::Normal(copied)) = failure_source(item) else {
			panic!("a directory of the user's drive");
		};
		assert_eq!(copied.uuid(), source.uuid());
	}

	#[test]
	fn a_failed_shared_or_linked_directory_is_a_copy_source_again() {
		let shared = drive_dir(Uuid::from_u128(0x5d), "Shared");
		let role = SharingRole::Receiver(ShareInfo {
			email: "sharer@example.com".to_owned(),
			id: 7,
		});
		let item = AnyItemWithContext::from(FailedSource::Dir(ItemSourceDir::Shared(
			DirType::Dir(Cow::Owned(SharedDirectory {
				inner: shared.clone(),
			})),
			role.clone(),
		)));
		let ItemSource::Dir(ItemSourceDir::Shared(dir_back, role_back)) = failure_source(item)
		else {
			panic!("a shared directory");
		};
		assert_eq!(dir_back.uuid(), shared.uuid());
		assert_eq!(role_back, role);

		let linked = drive_dir(Uuid::from_u128(0x1d), "Linked");
		let link = DirPublicLink {
			link_uuid: Uuid::from_u128(0x11),
			link_key: MetaKey::V3(EncryptionKey::generate()),
			password: PasswordState::None,
			enable_download: true,
			salt: LinkPasswordSalt::None,
		};
		let item = AnyItemWithContext::from(FailedSource::Dir(ItemSourceDir::Linked(
			DirType::Dir(Cow::Owned(LinkedDirectory(linked.clone()))),
			link.clone(),
		)));
		let ItemSource::Dir(ItemSourceDir::Linked(dir_back, link_back)) = failure_source(item)
		else {
			panic!("a directory in a public link");
		};
		assert_eq!(dir_back.uuid(), linked.uuid());
		assert_eq!(link_back, link, "the link, with its key, comes back intact");
	}

	#[test]
	fn the_root_directory_cannot_be_copied() {
		let root = AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Root(
			Root::from(RootDirectory::new(Uuid::from_u128(0x7))),
		)));
		let error = ItemSource::try_from(root).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState);
	}

	#[test]
	fn a_name_given_for_a_copy_must_be_valid() {
		let entry = |name: &str| CopyEntry {
			item: AnyItemWithContext::from(FailedSource::File(Box::new(file()))),
			destination: AnyNormalDir::Dir(drive_dir(Uuid::from_u128(0xd), "Photos").into()),
			name: Some(name.to_owned()),
		};
		let error = CopyRequest::try_from(entry("a/b.txt")).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidName);
		let request = CopyRequest::try_from(entry("b.txt")).unwrap();
		assert_eq!(request.name.as_ref().map(AsRef::as_ref), Some("b.txt"));
	}

	#[test]
	fn an_update_reports_milliseconds_and_the_parts_of_its_errors() {
		let parent = drive_dir(Uuid::from_u128(0xd), "Photos");
		let failure = FailureInfo {
			source_uuid: Uuid::from_u128(0xf),
			source_path: "/a.txt".to_owned(),
			dest_parent_dir: DirType::Dir(Cow::Owned(parent.clone())),
			dest_name: "a.txt".to_owned(),
			stage: CopyStage::Upload,
			error: Arc::new(Error::custom(ErrorKind::MaxStorageReached, "full")),
			affected_files: 1,
			affected_bytes: 10,
		};
		let update = CopyUpdate::from(copy::CopyUpdate {
			events: vec![copy::CopyEvent::FileFailed(failure.clone())],
			bytes_per_second: Some(100),
			eta: Some(Duration::from_millis(1500)),
			..update(2500)
		});
		assert_eq!(update.eta_ms, Some(1500));
		assert_eq!(update.active_time_ms, 2500);
		let [CopyEvent::FileFailed(info)] = update.events.as_slice() else {
			panic!("one failed file");
		};
		assert_eq!(info.error.kind(), ErrorKind::MaxStorageReached);
		let AnyNormalDir::Dir(parent_dir) = &info.dest_parent_dir else {
			panic!("the directory the file was to be created in");
		};
		assert_eq!(
			DirType::<'static, Normal>::from(AnyNormalDir::Dir(parent_dir.clone())).uuid(),
			parent.uuid(),
			"a retry can target the parent without looking it up"
		);
		assert_eq!(info.dest_parent, parent.uuid());
		assert_eq!(info.source_uuid, failure.source_uuid);
		assert_eq!(info.affected_bytes, 10);
	}

	#[test]
	fn a_report_carries_why_the_copy_ended() {
		let error = Arc::new(Error::custom(ErrorKind::Cancelled, "copy cancelled"));
		let cancelled = CopyReport::from(CopyFailed {
			report: copy::CopyReport::default(),
			error: Arc::clone(&error),
		});
		let reported = cancelled.error.expect("a cancelled copy says why it ended");
		assert!(Arc::ptr_eq(&reported, &error), "the SDK error itself");
		assert_eq!(reported.kind(), ErrorKind::Cancelled);
		let done = CopyReport::from(copy::CopyReport::default());
		assert!(done.error.is_none());
	}

	#[test]
	fn a_refused_copy_keeps_its_totals_and_counts() {
		let totals = PlanTotals {
			dirs: 1,
			files: 2,
			bytes: 1024,
		};
		let counts = ItemCounts {
			dirs_not_attempted: 1,
			files_not_attempted: 2,
			bytes_not_attempted: 1024,
			..ItemCounts::default()
		};
		let refused = CopyReport::from(CopyFailed {
			report: copy::CopyReport {
				totals,
				counts,
				..copy::CopyReport::default()
			},
			error: Arc::new(Error::custom(ErrorKind::MaxStorageReached, "full")),
		});
		assert_eq!((refused.totals, refused.counts), (totals, counts));
		assert_eq!(
			refused.error.map(|error| error.kind()),
			Some(ErrorKind::MaxStorageReached)
		);
		let update = CopyUpdate::from(copy::CopyUpdate {
			phase: CopyPhase::Failed,
			totals,
			counts,
			..update(0)
		});
		assert_eq!((update.totals, update.counts), (totals, counts));
	}

	impl CopyItemsCallback for Recorder {
		fn on_top_level_planned(&self, items: Vec<CopyPlannedItem>) {
			for item in items {
				self.push(item.request);
			}
		}

		fn on_top_level_created(&self, item: CopiedTopLevelItem) {
			self.push(item.request);
		}

		fn on_update(&self, update: CopyUpdate) {
			self.push(update.active_time_ms);
		}
	}

	fn planned(request: u64) -> PlannedTopLevelItem {
		PlannedTopLevelItem {
			request: request as usize,
			source_uuid: Uuid::from_u128(0x5),
			dest_uuid: Uuid::from_u128(0xde),
			dest_parent: PARENT,
			name: "a".to_owned(),
			is_dir: false,
		}
	}

	fn update(millis: u64) -> copy::CopyUpdate {
		copy::CopyUpdate {
			phase: CopyPhase::CopyingFiles,
			run_state: RunState::Running,
			scan: ScanProgress::default(),
			totals: PlanTotals::default(),
			counts: ItemCounts::default(),
			active: Vec::new(),
			events: Vec::new(),
			bytes_per_second: None,
			eta: None,
			active_time: Duration::from_millis(millis),
		}
	}

	#[test]
	fn callbacks_are_delivered_in_order_until_the_job_lets_go() {
		delivered_in_order(
			|recorder, delivery| uniffi_impl::deliver_copy(recorder, delivery),
			|sender| {
				let channel = CopyChannel(sender);
				let mut sent = Vec::new();
				for i in 0..300 {
					match i % 3 {
						0 => channel.on_top_level_planned(vec![planned(i)]),
						1 => channel.on_update(update(i)),
						_ => channel.on_top_level_created(CopiedTopLevel {
							request: i as usize,
							source_uuid: Uuid::from_u128(0x5),
							item: NonRootItemType::Dir(Cow::Owned(drive_dir(
								Uuid::from_u128(0xde),
								"a",
							))),
						}),
					}
					sent.push(i);
				}
				sent
			},
		);
	}
}
