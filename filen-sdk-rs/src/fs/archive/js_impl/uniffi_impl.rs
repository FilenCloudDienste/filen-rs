use std::sync::Arc;

use crate::{
	Error,
	auth::JsClient,
	js::{AnyFile, AnyItemWithContext, AnyNormalDir, File, ManagedFuture, spawn_ordered_dispatch},
};

use super::{
	CompressCall, CompressDelivery, CompressFormat, CompressReport, CompressUpdate, ExpansionLimit,
	ExtractConfig, ExtractDelivery, ExtractInto, ExtractReport, ExtractUpdate,
	ExtractedTopLevelItem, SourceDisposal, compress_job, extract_job, extract_request, password,
};

/// Receives an extract's progress, in the order the extract made it, before the call
/// returns.
#[uniffi::export(with_foreign)]
pub trait ExtractArchiveCallback: Send + Sync {
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps.
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevelItem>);
	fn on_update(&self, update: ExtractUpdate);
}

/// Receives a compress's progress, in the order the compress made it, before the call
/// returns.
#[uniffi::export(with_foreign)]
pub trait CompressItemsCallback: Send + Sync {
	/// The archive is registered in the destination.
	fn on_archive_created(&self, archive: File);
	fn on_update(&self, update: CompressUpdate);
}

#[derive(uniffi::Record, Default)]
pub struct ExtractArchiveConfig {
	/// Storage still free on the account, if known: a zip or 7z stating more fails before
	/// anything is written; a streaming archive (tar, one compressed file) fails once it has
	/// written that much, keeping what it extracted.
	#[uniffi(default = None)]
	pub max_bytes: Option<u64>,
	/// Most items to create.
	#[uniffi(default = None)]
	pub max_items: Option<u64>,
	/// The guard against decompression bombs; `None` keeps the SDK's (1000 times the
	/// archive, at least 256 MiB).
	#[uniffi(default = None)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Removes the archive once everything in it was extracted and verified.
	#[uniffi(default = None)]
	pub dispose: Option<SourceDisposal>,
}

#[derive(uniffi::Record)]
pub struct CompressItemsConfig {
	pub format: CompressFormat,
	/// Storage still free on the account, if known.
	#[uniffi(default = None)]
	pub max_bytes: Option<u64>,
	/// Removes the items once the archive is registered and verified. The archive is not read
	/// back first: with an encrypted format, have the user confirm the password (type it twice)
	/// before removing anything for good, as a mistyped one leaves an archive nobody can open.
	#[uniffi(default = None)]
	pub dispose: Option<SourceDisposal>,
}

#[uniffi::export]
impl JsClient {
	/// Extracts `archive` (zip, 7z, tar and its compressed forms, or one compressed file)
	/// into `destination`, entirely on this device: the archive is downloaded, decrypted and
	/// decoded as a stream, and every entry uploaded as a new item. A name taken at the
	/// destination is kept, and the entry is named `name (1)`, ...
	///
	/// `password` opens an encrypted archive; without one the extract fails before anything is
	/// created. A wrong one is found before anything is created too when the archive has an
	/// encrypted entry small enough to check it on (16 MiB), else as the first encrypted entry
	/// is read; folders created by then go to the trash. The report is returned whether the extract
	/// completed, was cancelled or failed. Only an abort through `managed_future` gets
	/// that report: cancelling the calling coroutine or task drops the call, and with it the
	/// report (the job is stopped at once).
	#[allow(clippy::too_many_arguments)]
	pub async fn extract_archive(
		&self,
		archive: AnyFile,
		destination: AnyNormalDir,
		into: ExtractInto,
		config: ExtractArchiveConfig,
		password: Option<String>,
		callback: Arc<dyn ExtractArchiveCallback>,
		managed_future: ManagedFuture,
	) -> Result<ExtractReport, Error> {
		// wrapped first, so an argument refused below still drops it wiped
		let password = self::password(password)?;
		let request = extract_request(archive, destination, into, config.dispose)?;
		let config = ExtractConfig {
			max_bytes: config.max_bytes,
			max_items: config.max_items,
			expansion_limit: config
				.expansion_limit
				.or(ExtractConfig::default().expansion_limit),
			password,
			// the bindings do not expose the option yet: the SDK's default
			skip_mac_metadata: ExtractConfig::default().skip_mac_metadata,
		};
		let client = self.inner();
		// the foreign callbacks run on the dispatch thread, in order; waiting for them is no
		// part of the job, so a cancel's grace never cuts off a report already made
		let (sender, delivered) = spawn_ordered_dispatch(move |delivery| match delivery {
			ExtractDelivery::TopLevelCreated(items) => callback.on_top_level_created(items),
			ExtractDelivery::Update(update) => callback.on_update(update),
		});
		let result = managed_future
			.into_js_managed_commander_job(move |control| {
				extract_job(client, request, config, sender, control)
			})
			.await;
		// the job has ended and dropped its sender: this returns once all it reported was
		// delivered
		let _ = delivered.await;
		result
	}

	/// Compresses `items` into a new archive `name` in `destination`, entirely on this
	/// device. `name` has to end in the format's extension (see `archive_extension`); a name
	/// taken at the destination is kept, and the archive is named `name (1).ext`, ...
	///
	/// `password` is required exactly when the format is encrypted. The report is returned
	/// whether the compress completed, was cancelled or failed; a compress that did not
	/// complete leaves nothing in the drive. Only an abort through `managed_future` gets
	/// that report: cancelling the calling coroutine or task drops the call, and with it the
	/// report (the job is stopped at once).
	#[allow(clippy::too_many_arguments)]
	pub async fn compress_items(
		&self,
		items: Vec<AnyItemWithContext>,
		destination: AnyNormalDir,
		name: String,
		config: CompressItemsConfig,
		password: Option<String>,
		callback: Arc<dyn CompressItemsCallback>,
		managed_future: ManagedFuture,
	) -> Result<CompressReport, Error> {
		let call = CompressCall::new(
			items,
			destination,
			&name,
			config.format,
			config.max_bytes,
			config.dispose,
			self::password(password)?,
		)?;
		let client = self.inner();
		let (sender, delivered) = spawn_ordered_dispatch(move |delivery| match delivery {
			CompressDelivery::ArchiveCreated(archive) => callback.on_archive_created(archive),
			CompressDelivery::Update(update) => callback.on_update(update),
		});
		let result = managed_future
			.into_js_managed_commander_job(move |control| {
				compress_job(client, call, sender, control)
			})
			.await;
		let _ = delivered.await;
		result
	}
}
