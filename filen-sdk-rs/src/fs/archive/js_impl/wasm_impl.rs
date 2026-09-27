use std::sync::Arc;

use filen_macros::js_type;
use wasm_bindgen::prelude::wasm_bindgen;
use web_sys::js_sys;

use crate::{
	Error,
	auth::JsClient,
	js::{
		AnyFile, AnyItemWithContext, AnyNormalDir, ManagedFuture, call_callback,
		spawn_local_dispatch,
	},
};

use super::{
	ArchiveEntryId, ArchiveListing, Client, CompressCall, CompressConfig, CompressDelivery,
	CompressFormat, CompressReport, ExpansionLimit, ExtractConfig, ExtractDelivery, ExtractReport,
	ExtractRequest, ExtractRoot, ListDelivery, RemoteFileType, SourceDisposal, compress_job,
	entries_request, extract_config, extract_job, extract_request, list_job, password,
};

#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveParams {
	pub archive: AnyFile,
	pub destination: AnyNormalDir,
	pub root: ExtractRoot,
	/// Storage still free on the account, if known: an extraction whose files would reach it
	/// fails with `MaxStorageReached`. A zip or 7z states its files' sizes in its index, so one
	/// stating that much fails before anything is created; a tar or single compressed file is
	/// checked as it is read, keeping what was extracted so far.
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
	/// Leaves out the metadata macOS writes beside files: everything in a `__MACOSX` folder
	/// and AppleDouble `._name` files, reported skipped for `macMetadata`. Left out on purpose,
	/// they keep nothing from removing the archive afterwards. `true` when left out; `false`
	/// extracts them as ordinary files.
	#[serde(default)]
	#[tsify(optional)]
	pub skip_mac_metadata: Option<bool>,
	/// Removes the archive once everything in it was extracted and verified.
	#[serde(default)]
	#[tsify(optional)]
	pub dispose: Option<SourceDisposal>,
	#[tsify(type = "(update: ExtractUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps.
	#[tsify(type = "(items: ExtractedTopLevelItem[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_top_level_created: Option<js_sys::Function>,
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveEntriesParams {
	pub archive: AnyFile,
	/// The entries to extract, from `listArchive` or a failure's `entry`.
	pub entries: Vec<ArchiveEntryId>,
	/// The directory of the archive the entries' paths are taken from, as drive names
	/// separated by `/`; empty to keep the archive's paths.
	pub base: String,
	pub destination: AnyNormalDir,
	pub root: ExtractRoot,
	/// Storage still free on the account, if known: an extraction whose files would reach it
	/// fails with `MaxStorageReached`. A zip or 7z states its files' sizes in its index, so one
	/// stating that much fails before anything is created; a tar or single compressed file is
	/// checked as it is read, keeping what was extracted so far.
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
	/// Leaves out the metadata macOS writes beside files: everything in a `__MACOSX` folder
	/// and AppleDouble `._name` files, reported skipped for `macMetadata`. `true` when left
	/// out; `false` extracts them as ordinary files.
	#[serde(default)]
	#[tsify(optional)]
	pub skip_mac_metadata: Option<bool>,
	#[tsify(type = "(update: ExtractUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps.
	#[tsify(type = "(items: ExtractedTopLevelItem[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_top_level_created: Option<js_sys::Function>,
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

#[js_type(import, no_ser, no_default)]
pub struct ListArchiveParams {
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
	/// every one of them, also past what the listing keeps.
	#[tsify(type = "(entries: ArchiveEntry[]) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_entries: Option<js_sys::Function>,
	#[tsify(type = "(update: ListUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	// A direct (never flattened) field, so the abort and pause signals stay live JS values.
	#[serde(default)]
	pub managed_future: ManagedFuture,
}

#[js_type(import, no_ser, no_default)]
pub struct CompressItemsParams {
	pub items: Vec<AnyItemWithContext>,
	pub destination: AnyNormalDir,
	/// The archive's name, ending in the format's extension (see `archiveExtension`).
	pub name: String,
	pub format: CompressFormat,
	/// Storage still free on the account, if known: a bare tar that would reach it is refused
	/// up front with `MaxStorageReached`, its size in the report's `neededBytes`; any other
	/// format as soon as its written bytes would, leaving nothing behind.
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
	#[tsify(type = "(update: CompressUpdate) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_update: Option<js_sys::Function>,
	/// The archive is registered in the destination.
	#[tsify(type = "(archive: File) => void", optional)]
	#[serde(default, deserialize_with = "crate::js::optional_function")]
	pub on_archive_created: Option<js_sys::Function>,
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
			ExtractDelivery::Update(update) => call_callback(self.on_update.as_ref(), &update),
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
	let (sender, delivered) = spawn_local_dispatch(move |delivery| callbacks.deliver(delivery));
	let result = managed_future
		.into_js_managed_commander_job(move |control| {
			extract_job(client, request, config, sender, control)
		})?
		.await;
	// the job has ended and dropped its sender: everything it reported reaches its callbacks
	// before the result does
	let _ = delivered.await;
	result
}

async fn run_list(
	client: Arc<Client>,
	archive: RemoteFileType<'static>,
	config: ExtractConfig,
	on_entries: Option<js_sys::Function>,
	on_update: Option<js_sys::Function>,
	managed_future: ManagedFuture,
) -> Result<ArchiveListing, Error> {
	let (sender, delivered) = spawn_local_dispatch(move |delivery| match delivery {
		ListDelivery::Entries(entries) => call_callback(on_entries.as_ref(), &entries),
		ListDelivery::Update(update) => call_callback(on_update.as_ref(), &update),
	});
	let result = managed_future
		.into_js_managed_commander_job(move |control| {
			list_job(client, archive, config, sender, control)
		})?
		.await;
	let _ = delivered.await;
	result
}

async fn run_compress(
	client: Arc<Client>,
	call: CompressCall,
	on_update: Option<js_sys::Function>,
	on_archive_created: Option<js_sys::Function>,
	managed_future: ManagedFuture,
) -> Result<CompressReport, Error> {
	let (sender, delivered) = spawn_local_dispatch(move |delivery| match delivery {
		CompressDelivery::ArchiveCreated(archive) => {
			call_callback(on_archive_created.as_ref(), &archive)
		}
		CompressDelivery::Update(update) => call_callback(on_update.as_ref(), &update),
	});
	let result = managed_future
		.into_js_managed_commander_job(move |control| compress_job(client, call, sender, control))?
		.await;
	let _ = delivered.await;
	result
}

#[wasm_bindgen(js_class = "Client")]
impl JsClient {
	/// The memory for one archive job's codec state in effect, in bytes (see
	/// `JsClientConfig.archiveCodecMemBudget`): pass it to `archiveMaxLevel`, or compare
	/// `archiveEncoderMemory` with it, to offer only what this device runs.
	#[wasm_bindgen(js_name = "archiveCodecMemBudget")]
	pub fn archive_codec_mem_budget(&self) -> u64 {
		self.inner_ref().archive_config().codec_mem_budget
	}

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
		// wrapped first, so an argument refused below still drops it wiped
		let password = self::password(password)?;
		let request = extract_request(
			params.archive,
			params.destination,
			params.root,
			params.dispose,
		)?;
		let config = extract_config(
			params.max_bytes,
			params.max_items,
			params.expansion_limit,
			params.skip_mac_metadata,
			password,
		);
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
	/// gives the `base` and destination that put it where it was meant to land. An entry of
	/// another archive fails the extract before anything runs. A zip's or 7z's entries are
	/// checked against its index before anything is created; a tar's are only known as it is
	/// read, so one not below `base` fails the extract when it is reached, and one the tar does
	/// not hold once it is read to its end, what was extracted until then staying. A chosen
	/// directory of a tar brings what the tar stores after it below it (every tool stores a
	/// directory before its contents); a zip's or 7z's everything below it. The archive is
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
			params.base,
			params.destination,
			params.root,
		)?;
		let config = extract_config(
			params.max_bytes,
			params.max_items,
			params.expansion_limit,
			params.skip_mac_metadata,
			password,
		);
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
		let config = extract_config(
			None,
			None,
			params.expansion_limit,
			params.skip_mac_metadata,
			password,
		);
		run_list(
			self.inner(),
			archive,
			config,
			params.on_entries,
			params.on_update,
			params.managed_future,
		)
		.await
	}

	/// Compresses items into a new archive in a directory, entirely in this browser. `name`
	/// has to end in the format's extension (see `archiveExtension`); a name taken at the
	/// destination is kept, and the archive is named `name (1).ext`, ...
	///
	/// `password` is required exactly when the format is encrypted. A format whose encoder
	/// needs more than `archiveCodecMemBudget` is refused, as an invalid argument. The report
	/// is returned whether the compress completed, was cancelled or failed; a compress that did
	/// not complete leaves nothing in the drive.
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
		run_compress(
			self.inner(),
			call,
			params.on_update,
			params.on_archive_created,
			params.managed_future,
		)
		.await
	}
}
