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
	Client, CompressCall, CompressConfig, CompressDelivery, CompressFormat, CompressReport,
	ExpansionLimit, ExtractConfig, ExtractDelivery, ExtractInto, ExtractReport, ExtractRequest,
	SourceDisposal, compress_job, extract_job, extract_request, password,
};

#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveParams {
	pub archive: AnyFile,
	pub destination: AnyNormalDir,
	pub into: ExtractInto,
	/// Storage still free on the account, if known: an extraction whose files would reach it
	/// fails with `MaxStorageReached`. A zip or 7z states its files' sizes in its index, so one
	/// stating that much fails before anything is created; a tar or single compressed file is
	/// checked as it is read, keeping what was extracted so far.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_bytes: Option<u64>,
	/// Most items to create.
	#[serde(default)]
	#[tsify(type = "number | bigint", optional)]
	pub max_items: Option<u64>,
	/// The guard against decompression bombs; when left out, the SDK's (1000 times the
	/// archive, at least 256 MiB).
	#[serde(default)]
	#[tsify(optional)]
	pub expansion_limit: Option<ExpansionLimit>,
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

async fn run_extract(
	client: Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
	on_update: Option<js_sys::Function>,
	on_top_level_created: Option<js_sys::Function>,
	managed_future: ManagedFuture,
) -> Result<ExtractReport, Error> {
	let (sender, delivered) = spawn_local_dispatch(move |delivery| match delivery {
		ExtractDelivery::TopLevelCreated(items) => {
			call_callback(on_top_level_created.as_ref(), &items)
		}
		ExtractDelivery::Update(update) => call_callback(on_update.as_ref(), &update),
	});
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
	/// is read; folders created by then go to the trash. The report is returned whether the extract
	/// completed, was cancelled or failed.
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
			params.into,
			params.dispose,
		)?;
		let config = ExtractConfig {
			max_bytes: params.max_bytes,
			max_items: params.max_items,
			expansion_limit: params
				.expansion_limit
				.or(ExtractConfig::default().expansion_limit),
			password,
			// the bindings do not expose the option yet: the SDK's default
			skip_mac_metadata: ExtractConfig::default().skip_mac_metadata,
		};
		run_extract(
			self.inner(),
			request,
			config,
			params.on_update,
			params.on_top_level_created,
			params.managed_future,
		)
		.await
	}

	/// Compresses items into a new archive in a directory, entirely in this browser. `name`
	/// has to end in the format's extension (see `archiveExtension`); a name taken at the
	/// destination is kept, and the archive is named `name (1).ext`, ...
	///
	/// `password` is required exactly when the format is encrypted. The report is returned
	/// whether the compress completed, was cancelled or failed; a compress that did not
	/// complete leaves nothing in the drive.
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
