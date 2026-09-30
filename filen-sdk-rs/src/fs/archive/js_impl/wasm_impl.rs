use std::sync::Arc;

use filen_macros::js_type;
use wasm_bindgen::prelude::wasm_bindgen;
use web_sys::js_sys;

use crate::{
	Error,
	auth::JsClient,
	js::{AnyFile, AnyItemWithContext, AnyNormalDir, ManagedFuture, call_callback},
};

use super::{
	ArchiveEntryId, ArchiveListing, Client, CompressCall, CompressConfig, CompressDelivery,
	CompressFormat, CompressReport, CompressUpdate, ExpansionLimit, ExtractConfig, ExtractDelivery,
	ExtractReport, ExtractRequest, ExtractRoot, ExtractSettings, ExtractUpdate, ListDelivery,
	RemoteFileType, SourceDisposal, compress_job, entries_request, extract_job, extract_request,
	list_job, password,
};

/// What `extractArchive` extracts, where to, and how.
#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveParams {
	/// The archive: any file the client can read.
	pub archive: AnyFile,
	/// An existing directory of the user's drive.
	pub destination: AnyNormalDir,
	/// Whether the entries go into a new folder or straight into `destination`.
	pub root: ExtractRoot,
	/// Storage still free on the account, if known. A job that needs more fails with
	/// `MaxStorageReached`; one that needs exactly this much fits. A zip or 7z states its files'
	/// sizes in its index, so one stating more fails before anything is created; a tar or single
	/// compressed file is checked as it is read, keeping what was extracted so far.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_bytes: Option<u64>,
	/// Most directories and files to create; an archive with more fails with
	/// `ArchiveTooLarge`.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_items: Option<u64>,
	/// The guard against decompression bombs, which also bounds what a tar's hard links copy
	/// in all (past it: `ArchiveTooLarge`); when left out, the SDK's (1000 times the archive,
	/// at least 256 MiB).
	#[serde(default)]
	#[tsify(optional)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Leaves out the metadata macOS writes beside files, reported skipped for `macMetadata`:
	/// AppleDouble files (named `._name` or kept in a `__MACOSX` folder, told by their first
	/// bytes), a tar's hard links to them, and the `__MACOSX` folders that hold nothing else (one
	/// holding anything of the user's, or nothing, is created). Left out on purpose, they keep
	/// nothing from removing the archive afterwards. `true` when left out; `false` extracts
	/// them as ordinary files.
	#[serde(default)]
	#[tsify(optional)]
	pub skip_mac_metadata: Option<bool>,
	/// Removes the archive once everything in it was extracted and verified. A tar or a single
	/// compressed file is verified by its hash, read front to back; a zip or a 7z by every
	/// entry's own checksum and every entry having been extracted, not by a whole-archive hash.
	/// macOS metadata left out (`skipMacMetadata`) does not keep it: removed for good, the
	/// archive takes what was left out with it.
	#[serde(default)]
	#[tsify(optional)]
	pub dispose: Option<SourceDisposal>,
	/// The job's progress, throttled; the last one comes once the job ended.
	#[tsify(type = "(update: ExtractUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps. A folder among them goes to the trash again when a wrong password shows only
	/// once entries were read, before any file was extracted: an update's `topLevelTrashed`
	/// event tells which.
	#[tsify(type = "(items: ExtractedTopLevelItem[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_top_level_created: Option<js_sys::Function>,
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

/// What `extractArchiveEntries` extracts, where to, and how.
#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveEntriesParams {
	/// The archive: any file the client can read.
	pub archive: AnyFile,
	/// The entries to extract, from `listArchive` or a failure's `entry`.
	pub entries: Vec<ArchiveEntryId>,
	/// The directory of the archive the entries' paths are taken from, as drive names
	/// separated by `/`; empty to keep the archive's paths.
	pub base: String,
	/// An existing directory of the user's drive.
	pub destination: AnyNormalDir,
	/// Whether the entries go into a new folder or straight into `destination`.
	pub root: ExtractRoot,
	/// Storage still free on the account, if known. A job that needs more fails with
	/// `MaxStorageReached`; one that needs exactly this much fits. A zip or 7z states its files'
	/// sizes in its index, so one stating more fails before anything is created; a tar or single
	/// compressed file is checked as it is read, keeping what was extracted so far.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_bytes: Option<u64>,
	/// Most directories and files to create; an archive with more fails with
	/// `ArchiveTooLarge`.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_items: Option<u64>,
	/// The guard against decompression bombs, which also bounds what a tar's hard links copy
	/// in all (past it: `ArchiveTooLarge`); when left out, the SDK's (1000 times the archive,
	/// at least 256 MiB).
	#[serde(default)]
	#[tsify(optional)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Leaves out the metadata macOS writes beside files, reported skipped for `macMetadata`:
	/// AppleDouble files (named `._name` or kept in a `__MACOSX` folder, told by their first
	/// bytes), a tar's hard links to them, and the `__MACOSX` folders that hold nothing else (one
	/// holding anything of the user's, or nothing, is created). `true` when left out; `false`
	/// extracts them as ordinary files.
	#[serde(default)]
	#[tsify(optional)]
	pub skip_mac_metadata: Option<bool>,
	/// The job's progress, throttled; the last one comes once the job ended.
	#[tsify(type = "(update: ExtractUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps. A folder among them goes to the trash again when a wrong password shows only
	/// once entries were read, before any file was extracted: an update's `topLevelTrashed`
	/// event tells which.
	#[tsify(type = "(items: ExtractedTopLevelItem[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_top_level_created: Option<js_sys::Function>,
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

/// What `listArchive` lists, and how.
#[js_type(import, no_ser, no_default)]
pub struct ListArchiveParams {
	/// The archive: any file the client can read.
	pub archive: AnyFile,
	/// The guard against decompression bombs an extraction would run under (see
	/// `ExtractArchiveParams.expansionLimit`); when left out, the SDK's.
	#[serde(default)]
	#[tsify(optional)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Lists macOS metadata as an extraction with the same setting skips it (see
	/// `ExtractArchiveParams.skipMacMetadata`); `true` when left out.
	#[serde(default)]
	#[tsify(optional)]
	pub skip_mac_metadata: Option<bool>,
	/// Entries, in batches as they are read, each batch before the update that counts it:
	/// every one of them, also past what the listing keeps, unless this is still busy with 16
	/// MiB of earlier ones, counting the entries as well as their text (see
	/// `ListUpdate.undeliveredEntries`).
	#[tsify(type = "(entries: ArchiveEntry[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_entries_batch: Option<js_sys::Function>,
	/// The listing's progress, throttled; the last one comes once the listing ended.
	#[tsify(type = "(update: ListUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// The signals that pause and cancel the job.
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

/// What `compressItems` compresses, into what, where to, and how.
#[js_type(import, no_ser, no_default)]
pub struct CompressItemsParams {
	/// The items to compress: any items the client can read, or with `dispose` items of the
	/// user's own drive.
	pub items: Vec<AnyItemWithContext>,
	/// An existing directory of the user's drive, for the archive.
	pub destination: AnyNormalDir,
	/// The archive's name, ending in the format's extension (see `archiveExtension`).
	pub name: String,
	/// What the archive is written as.
	pub format: CompressFormat,
	/// Storage still free on the account, if known. A job that needs more fails with
	/// `MaxStorageReached`; one that needs exactly this much fits. A bare tar that needs more is
	/// refused up front, its size in the report's `neededBytes`; any other format as soon as its
	/// written bytes would pass it, leaving nothing behind.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_bytes: Option<u64>,
	/// Removes the items once the archive is registered and verified. Before anything is
	/// deleted for good, the archive is read back from the server as extracting would read it
	/// (phase `verifying`, its progress in `counts.bytesVerified`); trashing reads nothing back.
	/// The read back uses the password the archive was written with, so it cannot tell a
	/// mistyped one: with an encrypted format, have the user confirm the password (type it
	/// twice) before removing anything for good.
	#[serde(default)]
	#[tsify(optional)]
	pub dispose: Option<SourceDisposal>,
	/// The job's progress, throttled; the last one comes once the job ended.
	#[tsify(type = "(update: CompressUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// The archive is registered in the destination.
	#[tsify(type = "(archive: File) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_archive_created: Option<js_sys::Function>,
	/// The signals that pause and cancel the job.
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

struct ExtractCallbacks {
	on_update: Option<js_sys::Function>,
	on_top_level_created: Option<js_sys::Function>,
}

impl ExtractCallbacks {
	fn deliver(&self, delivery: ExtractDelivery) {
		match delivery {
			ExtractDelivery::TopLevelCreated(items) => {
				call_callback(self.on_top_level_created.as_ref(), &items)
			}
			ExtractDelivery::Update(update) => {
				call_callback(self.on_update.as_ref(), &ExtractUpdate::from(update))
			}
		}
	}
}

struct ListCallbacks {
	on_entries_batch: Option<js_sys::Function>,
	on_update: Option<js_sys::Function>,
}

impl ListCallbacks {
	fn deliver(&self, delivery: ListDelivery) {
		match delivery {
			// the batch's text counts as queued until the callback has returned
			ListDelivery::Entries(entries, _queued) => {
				call_callback(self.on_entries_batch.as_ref(), &entries)
			}
			ListDelivery::Update(update) => call_callback(self.on_update.as_ref(), &update),
		}
	}
}

struct CompressCallbacks {
	on_update: Option<js_sys::Function>,
	on_archive_created: Option<js_sys::Function>,
}

impl CompressCallbacks {
	fn deliver(&self, delivery: CompressDelivery) {
		match delivery {
			CompressDelivery::ArchiveCreated(archive) => {
				call_callback(self.on_archive_created.as_ref(), &archive)
			}
			CompressDelivery::Update(update) => {
				call_callback(self.on_update.as_ref(), &CompressUpdate::from(update))
			}
		}
	}
}

async fn run_extract(
	client: Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
	callbacks: ExtractCallbacks,
	managed_future: ManagedFuture,
) -> Result<ExtractReport, Error> {
	managed_future
		.into_ordered_job(
			move |delivery| callbacks.deliver(delivery),
			move |sender, control| extract_job(client, request, config, sender, control),
		)
		.await
		.map(ExtractReport::new)
}

#[wasm_bindgen(js_class = "Client")]
impl JsClient {
	/// Extracts an archive (zip, 7z, tar and its compressed forms, or one compressed file)
	/// into a directory, entirely in this browser: the archive is downloaded, decrypted and
	/// decoded as a stream, and every entry uploaded as a new item. A name taken at the
	/// destination is kept, and the entry is named `name (1)`, ...
	///
	/// `password` opens an encrypted archive; without one the extract fails before anything is
	/// created. A wrong one is found before anything is created too when the archive has an
	/// encrypted entry small enough to check it on (16 MiB), else as the first encrypted entry
	/// is read; folders created by then go to the trash. The report is returned whether the
	/// extract completed, was cancelled or failed.
	#[wasm_bindgen(js_name = "extractArchive")]
	pub async fn extract_archive(
		&self,
		params: ExtractArchiveParams,
		password: Option<String>,
	) -> Result<ExtractReport, Error> {
		let password = self::password(password)?;
		let request = extract_request(
			params.archive,
			params.destination,
			params.root,
			params.dispose,
		)?;
		let config = ExtractSettings {
			max_bytes: params.max_bytes,
			max_items: params.max_items,
			expansion_limit: params.expansion_limit,
			skip_mac_metadata: params.skip_mac_metadata,
		}
		.into_config(password);
		let callbacks = ExtractCallbacks {
			on_update: params.on_update,
			on_top_level_created: params.on_top_level_created,
		};
		run_extract(
			self.inner(),
			request,
			config,
			callbacks,
			params.managed_future,
		)
		.await
	}

	/// `extractArchive` for some of the archive's entries: those `entries` names (from
	/// `listArchive`, or a failure's `entry`), everything below a directory among them, and the
	/// directories that hold them.
	///
	/// Each lands at its path in the archive less `base`, a directory of the archive as drive
	/// names separated by `/`: with `base` `photos`, the entry `photos/2024/a.jpg` lands at
	/// `2024/a.jpg` in the root. An empty `base` keeps the archive's paths. A failure's `retry`
	/// gives the `base` and destination that put it where it was meant to land. A call choosing
	/// no entry, an entry of another archive, or a `base` that is not drive names is refused
	/// before anything runs. A zip's or 7z's entries are checked against its index before
	/// anything is created; a tar's are only known as it is read, so one not below `base` fails
	/// the extract when it is reached, and one the tar does not hold once it is read to its end,
	/// what was extracted until then staying. A chosen directory of a tar brings what the tar
	/// stores after it below it (every tool stores a directory before its contents); a zip's or
	/// 7z's everything below it. The archive is
	/// never removed afterwards.
	#[wasm_bindgen(js_name = "extractArchiveEntries")]
	pub async fn extract_archive_entries(
		&self,
		params: ExtractArchiveEntriesParams,
		password: Option<String>,
	) -> Result<ExtractReport, Error> {
		let password = self::password(password)?;
		let request = entries_request(
			params.archive,
			params.entries,
			&params.base,
			params.destination,
			params.root,
		)?;
		let config = ExtractSettings {
			max_bytes: params.max_bytes,
			max_items: params.max_items,
			expansion_limit: params.expansion_limit,
			skip_mac_metadata: params.skip_mac_metadata,
		}
		.into_config(password);
		let callbacks = ExtractCallbacks {
			on_update: params.on_update,
			on_top_level_created: params.on_top_level_created,
		};
		run_extract(
			self.inner(),
			request,
			config,
			callbacks,
			params.managed_future,
		)
		.await
	}

	/// Lists an archive's entries without extracting any: what each one is, and what
	/// extracting it with the same settings would do with it (skip it, and why).
	///
	/// A zip's or 7z's index says nearly all: the index is read, and besides it only the
	/// smallest encrypted entry, as an extraction reads it, to check `password` (see the
	/// listing's `password`), and what tells a link's target: each unencrypted zip symlink's
	/// data, and a 7z link's when it is within the first 16 MiB of its folder (past that, a 7z
	/// link is listed without its target). A tar's members, or what a single compressed file
	/// decodes to, are only known by reading it all, which takes as long as downloading it; that
	/// is reported as it goes, and can be paused and cancelled through `managedFuture`. A
	/// listing takes one of the archive job slots, as an extract does. The listing is returned
	/// whether it completed, was cancelled or failed, with the entries read until then.
	#[wasm_bindgen(js_name = "listArchive")]
	pub async fn list_archive(
		&self,
		params: ListArchiveParams,
		password: Option<String>,
	) -> Result<ArchiveListing, Error> {
		let password = self::password(password)?;
		let archive = RemoteFileType::try_from(params.archive)?;
		let config = ExtractSettings {
			expansion_limit: params.expansion_limit,
			skip_mac_metadata: params.skip_mac_metadata,
			..ExtractSettings::default()
		}
		.into_list_config(password);
		let callbacks = ListCallbacks {
			on_entries_batch: params.on_entries_batch,
			on_update: params.on_update,
		};
		let client = self.inner();
		params
			.managed_future
			.into_ordered_job(
				move |delivery| callbacks.deliver(delivery),
				move |sender, control| list_job(client, archive, config, sender, control),
			)
			.await
			.map(|(result, undelivered)| ArchiveListing::new(result, undelivered))
	}

	/// Compresses items into a new archive in a directory, entirely in this browser. `name`
	/// has to end in the format's extension (see `archiveExtension`); a name taken at the
	/// destination is kept, and the archive is named `name (1).ext`, ...
	///
	/// `password` is required exactly when the format is encrypted. The call is refused before
	/// anything runs for a name without the format's extension (`InvalidName`), a level the
	/// format does not take or a password where none belongs (`InvalidState`), a missing one
	/// (`ArchivePasswordRequired`), and a format whose encoder needs more than
	/// `archiveCodecMemBudget` (`InsufficientMemory`). The report is returned whether the
	/// compress completed, was cancelled or failed; a compress that did not complete leaves
	/// nothing in the drive.
	#[wasm_bindgen(js_name = "compressItems")]
	pub async fn compress_items(
		&self,
		params: CompressItemsParams,
		password: Option<String>,
	) -> Result<CompressReport, Error> {
		let call = CompressCall::new(
			params.items,
			params.destination,
			&params.name,
			CompressConfig {
				format: params.format,
				max_bytes: params.max_bytes,
				password: self::password(password)?,
			},
			params.dispose,
			self.inner_ref().archive_config().codec_mem_budget,
		)?;
		let callbacks = CompressCallbacks {
			on_update: params.on_update,
			on_archive_created: params.on_archive_created,
		};
		let client = self.inner();
		params
			.managed_future
			.into_ordered_job(
				move |delivery| callbacks.deliver(delivery),
				move |sender, control| compress_job(client, call, sender, control),
			)
			.await
			.map(CompressReport::new)
	}
}
