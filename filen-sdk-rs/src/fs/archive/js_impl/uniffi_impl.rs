use std::sync::Arc;

use filen_macros::js_type;

use crate::{
	Error,
	auth::{Client, JsClient},
	js::{AnyFile, AnyItemWithContext, AnyNormalDir, File, ManagedFuture},
};

use super::{
	ArchiveEntry, ArchiveEntryId, ArchiveListing, CompressCall, CompressConfig, CompressDelivery,
	CompressFormat, CompressReport, CompressUpdate, ExpansionLimit, ExtractConfig, ExtractDelivery,
	ExtractReport, ExtractRequest, ExtractRoot, ExtractSettings, ExtractUpdate,
	ExtractedTopLevelItem, ListDelivery, ListUpdate, RemoteFileType, SourceDisposal, compress_job,
	entries_request, extract_job, extract_request, list_job, password,
};

/// Receives an extract's progress, in the order the extract made it, before the call
/// returns.
#[uniffi::export(with_foreign)]
pub trait ExtractArchiveCallback: Send + Sync {
	/// Top-level items created, in batches: every one of them, also past the 1000 the report
	/// keeps. A folder among them goes to the trash again when a wrong password shows only
	/// once entries were read, before any file was extracted: an update's
	/// `ExtractEvent::TopLevelTrashed` tells which.
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevelItem>);
	fn on_update(&self, update: ExtractUpdate);
}

/// Receives a listing's entries and progress, in the order the listing made them, before the
/// call returns.
#[uniffi::export(with_foreign)]
pub trait ListArchiveCallback: Send + Sync {
	/// Entries, in batches as they are read, each batch before the update that counts it:
	/// every one of them, also past what the listing keeps, unless this is still busy with 16
	/// MiB of earlier ones, counting the entries as well as their text (see
	/// `ListUpdate.undelivered_entries`).
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>);
	fn on_update(&self, update: ListUpdate);
}

/// Receives a compress's progress, in the order the compress made it, before the call
/// returns.
#[uniffi::export(with_foreign)]
pub trait CompressItemsCallback: Send + Sync {
	/// The archive is registered in the destination.
	fn on_archive_created(&self, archive: File);
	fn on_update(&self, update: CompressUpdate);
}

/// What `extract_archive` extracts into, and how.
#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveConfig {
	/// Where the archive's entries are created in the destination.
	pub root: ExtractRoot,
	/// Storage still free on the account, if known. A job that needs more fails with
	/// `MaxStorageReached`; one that needs exactly this much fits. A zip or 7z states its files'
	/// sizes in its index, so one stating more fails before anything is created; a tar or single
	/// compressed file is checked as it is read, keeping what was extracted so far.
	#[uniffi(default = None)]
	pub max_bytes: Option<u64>,
	/// Most directories and files to create; an archive with more fails with
	/// `ArchiveTooLarge`.
	#[uniffi(default = None)]
	pub max_items: Option<u64>,
	/// The guard against decompression bombs, which also bounds what a tar's hard links copy
	/// in all (past it: `ArchiveTooLarge`); `None` keeps the SDK's (1000 times the archive, at
	/// least 256 MiB).
	#[uniffi(default = None)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Leaves out the metadata macOS writes beside files, reported skipped for `MacMetadata`:
	/// AppleDouble files (named `._name` or kept in a `__MACOSX` folder, told by their first
	/// bytes), a tar's hard links to them, and the `__MACOSX` folders that hold nothing else (one
	/// holding anything of the user's, or nothing, is created). Left out on purpose, they keep
	/// nothing from removing the archive afterwards. `true` when `None`; `false` extracts them
	/// as ordinary files.
	#[uniffi(default = None)]
	pub skip_mac_metadata: Option<bool>,
	/// Removes the archive once everything in it was extracted and verified.
	#[uniffi(default = None)]
	pub dispose: Option<SourceDisposal>,
}

/// `ExtractArchiveConfig` for part of an archive, which is never removed afterwards, and the
/// entries to extract.
#[js_type(import, no_ser, no_default)]
pub struct ExtractArchiveEntriesConfig {
	/// The entries to extract, from `list_archive` or a failure's `entry`.
	pub entries: Vec<ArchiveEntryId>,
	/// The directory of the archive the entries' paths are taken from, as drive names separated
	/// by `/`; empty to keep the archive's paths.
	pub base: String,
	/// Where the entries are created in the destination.
	pub root: ExtractRoot,
	/// See `ExtractArchiveConfig.max_bytes`.
	#[uniffi(default = None)]
	pub max_bytes: Option<u64>,
	/// See `ExtractArchiveConfig.max_items`.
	#[uniffi(default = None)]
	pub max_items: Option<u64>,
	/// See `ExtractArchiveConfig.expansion_limit`.
	#[uniffi(default = None)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// See `ExtractArchiveConfig.skip_mac_metadata`.
	#[uniffi(default = None)]
	pub skip_mac_metadata: Option<bool>,
}

/// What a listing reports an extraction would do: the settings of `ExtractArchiveConfig` that
/// decide which entries it skips.
#[js_type(import, no_ser, no_default)]
#[derive(Default)]
pub struct ListArchiveConfig {
	/// The guard against decompression bombs an extraction would run under (see
	/// `ExtractArchiveConfig.expansion_limit`); `None` keeps the SDK's.
	#[uniffi(default = None)]
	pub expansion_limit: Option<ExpansionLimit>,
	/// Lists macOS metadata as an extraction with the same setting skips it (see
	/// `ExtractArchiveConfig.skip_mac_metadata`); `true` when `None`.
	#[uniffi(default = None)]
	pub skip_mac_metadata: Option<bool>,
}

/// What `compress_items` writes, and how.
#[js_type(import, no_ser, no_default)]
pub struct CompressItemsConfig {
	/// The archive's name, ending in the format's extension (see `archive_extension`).
	pub name: String,
	/// What the archive is written as.
	pub format: CompressFormat,
	/// Storage still free on the account, if known. A job that needs more fails with
	/// `MaxStorageReached`; one that needs exactly this much fits. A bare tar that needs more is
	/// refused up front, its size in the report's `neededBytes`; any other format as soon as its
	/// written bytes would pass it, leaving nothing behind.
	#[uniffi(default = None)]
	pub max_bytes: Option<u64>,
	/// Removes the items once the archive is registered and verified. Before anything is
	/// deleted for good, the archive is read back from the server as extracting would read it
	/// (phase `verifying`, its progress in `counts.bytesVerified`); trashing reads nothing back.
	/// The read back uses the password the archive was written with, so it cannot tell a
	/// mistyped one: with an encrypted format, have the user confirm the password (type it
	/// twice) before removing anything for good.
	#[uniffi(default = None)]
	pub dispose: Option<SourceDisposal>,
}

pub(super) fn deliver_extract(callback: &dyn ExtractArchiveCallback, delivery: ExtractDelivery) {
	match delivery {
		ExtractDelivery::TopLevelCreated(items) => callback.on_top_level_created(items),
		ExtractDelivery::Update(update) => callback.on_update(update.into()),
	}
}

pub(super) fn deliver_list(callback: &dyn ListArchiveCallback, delivery: ListDelivery) {
	match delivery {
		// the batch's text counts as queued until the callback has returned
		ListDelivery::Entries(entries, _queued) => callback.on_entries_batch(entries),
		ListDelivery::Update(update) => callback.on_update(update),
	}
}

pub(super) fn deliver_compress(callback: &dyn CompressItemsCallback, delivery: CompressDelivery) {
	match delivery {
		CompressDelivery::ArchiveCreated(archive) => callback.on_archive_created(archive),
		CompressDelivery::Update(update) => callback.on_update(update.into()),
	}
}

async fn run_extract(
	client: Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
	callback: Arc<dyn ExtractArchiveCallback>,
	managed_future: ManagedFuture,
) -> Result<ExtractReport, Error> {
	managed_future
		.into_ordered_job(
			move |delivery| deliver_extract(callback.as_ref(), delivery),
			move |sender, control| extract_job(client, request, config, sender, control),
		)
		.await
		.map(ExtractReport::new)
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
	/// is read; folders created by then go to the trash. The report is returned whether the
	/// extract completed, was cancelled or failed. Only an abort through `managed_future` gets
	/// that report: cancelling the calling coroutine or task drops the call, and with it the
	/// report (the job is stopped at once).
	pub async fn extract_archive(
		&self,
		archive: AnyFile,
		destination: AnyNormalDir,
		config: ExtractArchiveConfig,
		password: Option<String>,
		callback: Arc<dyn ExtractArchiveCallback>,
		managed_future: ManagedFuture,
	) -> Result<ExtractReport, Error> {
		// wrapped first, so an argument refused below still drops it wiped
		let password = self::password(password)?;
		let request = extract_request(archive, destination, config.root, config.dispose)?;
		let config = ExtractSettings {
			max_bytes: config.max_bytes,
			max_items: config.max_items,
			expansion_limit: config.expansion_limit,
			skip_mac_metadata: config.skip_mac_metadata,
		}
		.into_config(password);
		run_extract(self.inner(), request, config, callback, managed_future).await
	}

	/// `extract_archive` for some of the archive's entries: those the config's `entries` names
	/// (from `list_archive`, or a failure's `entry`), everything below a directory among them,
	/// and the directories that hold them.
	///
	/// Each lands at its path in the archive less the config's `base`, a directory of the
	/// archive as drive names separated by `/`: with `base` `photos`, the entry
	/// `photos/2024/a.jpg` lands at `2024/a.jpg` in the root. An empty `base` keeps the archive's paths. A failure's `retry`
	/// gives the `base` and destination that put it where it was meant to land. A call choosing
	/// no entry, an entry of another archive, or a `base` that is not drive names is refused
	/// before anything runs. A zip's or 7z's entries are checked against its index before
	/// anything is created; a tar's are only known as it is read, so one not below `base` fails
	/// the extract when it is reached, and one the tar does not hold once it is read to its end,
	/// what was extracted until then staying. A chosen directory of a tar brings what the tar
	/// stores after it below it (every tool stores a directory before its contents); a zip's or
	/// 7z's everything below it. The archive is
	/// never removed afterwards.
	pub async fn extract_archive_entries(
		&self,
		archive: AnyFile,
		destination: AnyNormalDir,
		config: ExtractArchiveEntriesConfig,
		password: Option<String>,
		callback: Arc<dyn ExtractArchiveCallback>,
		managed_future: ManagedFuture,
	) -> Result<ExtractReport, Error> {
		let password = self::password(password)?;
		let request = entries_request(
			archive,
			config.entries,
			&config.base,
			destination,
			config.root,
		)?;
		let config = ExtractSettings {
			max_bytes: config.max_bytes,
			max_items: config.max_items,
			expansion_limit: config.expansion_limit,
			skip_mac_metadata: config.skip_mac_metadata,
		}
		.into_config(password);
		run_extract(self.inner(), request, config, callback, managed_future).await
	}

	/// Lists `archive`'s entries without extracting any: what each one is, and what extracting
	/// it with the same settings would do with it (skip it, and why).
	///
	/// A zip's or 7z's index says nearly all: the index is read, and besides it only the
	/// smallest encrypted entry, as an extraction reads it, to check `password` (see the
	/// listing's `password`), and what tells a link's target: each unencrypted zip symlink's
	/// data, and a 7z link's when it is within the first 16 MiB of its folder (past that, a 7z
	/// link is listed without its target). A tar's members, or what a single compressed file
	/// decodes to, are only known by reading it all, which takes as long as downloading it; that
	/// is reported as it goes, and can be paused and cancelled through `managed_future`. A
	/// listing takes one of the archive job slots, as an extract does.
	///
	/// The listing is returned whether it completed, was cancelled or failed, with the entries
	/// read until then. Only an abort through `managed_future` gets it: cancelling the calling
	/// coroutine or task drops the call, and with it the listing.
	pub async fn list_archive(
		&self,
		archive: AnyFile,
		config: ListArchiveConfig,
		password: Option<String>,
		callback: Arc<dyn ListArchiveCallback>,
		managed_future: ManagedFuture,
	) -> Result<ArchiveListing, Error> {
		let password = self::password(password)?;
		let archive = RemoteFileType::try_from(archive)?;
		let config = ExtractSettings {
			expansion_limit: config.expansion_limit,
			skip_mac_metadata: config.skip_mac_metadata,
			..ExtractSettings::default()
		}
		.into_list_config(password);
		let client = self.inner();
		managed_future
			.into_ordered_job(
				move |delivery| deliver_list(callback.as_ref(), delivery),
				move |sender, control| list_job(client, archive, config, sender, control),
			)
			.await
			.map(|(result, undelivered)| ArchiveListing::new(result, undelivered))
	}

	/// Compresses `items` into a new archive in `destination`, named as the config says,
	/// entirely on this device. The name has to end in the format's extension (see
	/// `archive_extension`); a name taken at the destination is kept, and the archive is named
	/// `name (1).ext`, ...
	///
	/// `password` is required exactly when the format is encrypted. The call is refused before
	/// anything runs for a name without the format's extension (`InvalidName`), a level the
	/// format does not take or a password where none belongs (`InvalidState`), a missing one
	/// (`ArchivePasswordRequired`), and a format whose encoder needs more than
	/// `archive_codec_mem_budget` (`InsufficientMemory`). The report is returned whether the
	/// compress completed, was cancelled or failed; a compress that did not complete leaves
	/// nothing in the drive. Only an abort through `managed_future` gets that report: cancelling
	/// the calling coroutine or task drops the call, and with it the report (the job is stopped
	/// at once).
	pub async fn compress_items(
		&self,
		items: Vec<AnyItemWithContext>,
		destination: AnyNormalDir,
		config: CompressItemsConfig,
		password: Option<String>,
		callback: Arc<dyn CompressItemsCallback>,
		managed_future: ManagedFuture,
	) -> Result<CompressReport, Error> {
		let call = CompressCall::new(
			items,
			destination,
			&config.name,
			CompressConfig {
				format: config.format,
				max_bytes: config.max_bytes,
				password: self::password(password)?,
			},
			config.dispose,
			self.inner_ref().archive_config().codec_mem_budget,
		)?;
		let client = self.inner();
		managed_future
			.into_ordered_job(
				move |delivery| deliver_compress(callback.as_ref(), delivery),
				move |sender, control| compress_job(client, call, sender, control),
			)
			.await
			.map(CompressReport::new)
	}
}
