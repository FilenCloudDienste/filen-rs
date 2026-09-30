//! The codec side of compressing: runs on the codec worker, reads each source file through the
//! driver chunk by chunk, and writes the archive into a [`ChunkSink`] the driver uploads from.

use std::io::{self, Read, Write};

use chrono::{DateTime, Utc};

use crate::{Error, ErrorKind};

use super::{
	super::{
		encode::{StreamEncoder, open_encoder},
		password::ArchivePassword,
		sevenz::write::SevenZWriter,
		tar_iter::{TAR_BLOCK, USTAR_NAME_LEN},
		worker::{ChunkInput, ChunkSink, JobEnded, WorkerEvent, WorkerPort},
		zip::{
			crypto::AesStrength,
			write::{Encryption, ZipMethod, ZipWriter},
		},
	},
	CompressFormat,
};

/// An entry of the archive, in the order it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArchiveEntry {
	Dir {
		/// The path in the archive, `/`-separated, without a trailing `/`.
		path: String,
		modified: Option<DateTime<Utc>>,
	},
	File {
		/// Which input the codec asks the driver for.
		source: u32,
		path: String,
		/// The size the source was listed with; its data has to be exactly this long.
		size: u64,
		modified: Option<DateTime<Utc>>,
	},
}

pub(crate) struct CompressJob {
	pub(crate) format: CompressFormat,
	pub(crate) entries: Vec<ArchiveEntry>,
	/// For an encrypted format.
	pub(crate) password: Option<ArchivePassword>,
}

/// Writes the archive through `port`; returns its length. An error the driver caused (it went
/// away, or a fetch failed and stopped the job) comes back as [`ErrorKind::Cancelled`]; the
/// driver knows the real one.
pub(crate) fn compress(port: &WorkerPort, job: CompressJob) -> Result<u64, Error> {
	compress_with(port, job, ZipWriter::new)
}

/// [`compress`], a zip written through the writer `zip_writer` makes of the sink: a test's writes
/// zip64 records without writing 4 GiB.
fn compress_with<'p>(
	port: &'p WorkerPort,
	job: CompressJob,
	zip_writer: impl FnOnce(ChunkSink<'p>) -> ZipWriter<ChunkSink<'p>>,
) -> Result<u64, Error> {
	let sink = ChunkSink::new(port);
	match job.format {
		CompressFormat::Tar { compression: None } => {
			let mut tar = tar::Builder::new(sink);
			write_tar(port, &mut tar, job.entries)?;
			// writes the two end-of-archive blocks
			tar.into_inner().map_err(failure)?.finish().map_err(failure)
		}
		CompressFormat::Tar {
			compression: Some(compression),
		} => {
			let mut tar = tar::Builder::new(open_encoder(compression, sink)?);
			write_tar(port, &mut tar, job.entries)?;
			let encoder = tar.into_inner().map_err(failure)?;
			encoder.finish().map_err(failure)?.finish().map_err(failure)
		}
		CompressFormat::Zip { method, encryption } => {
			let password = with_password(encryption, job.password.as_ref(), "zip")?;
			write_zip(port, zip_writer(sink), job.entries, method, password)
		}
		CompressFormat::SevenZ {
			method,
			solid,
			encryption,
		} => {
			// a 7z's start points at its header, so its first chunk is sent last
			drop(sink);
			let password = with_password(encryption, job.password.as_ref(), "7z")?;
			let writer = SevenZWriter::new(ChunkSink::holding_head(port), method, solid, password)
				.map_err(failure)?;
			write_7z(port, writer, job.entries)
		}
		CompressFormat::Single { compression } => {
			let [ArchiveEntry::File { source, size, .. }] = job.entries[..] else {
				// the planner refuses any other shape before the codec starts
				return Err(Error::custom(
					ErrorKind::Internal,
					"a single compressed file is made of exactly one file",
				));
			};
			let mut encoder: Box<dyn StreamEncoder<ChunkSink<'_>>> =
				open_encoder(compression, sink)?;
			write_source(port, source, size, |data| io::copy(data, &mut encoder))?;
			encoder.finish().map_err(failure)?.finish().map_err(failure)
		}
	}
}

/// The password an encrypted `format` is written with, paired with its encryption settings.
fn with_password<'p, T>(
	encryption: Option<T>,
	password: Option<&'p ArchivePassword>,
	format: &str,
) -> Result<Option<(T, &'p ArchivePassword)>, Error> {
	match (encryption, password) {
		(None, _) => Ok(None),
		(Some(encryption), Some(password)) => Ok(Some((encryption, password))),
		(Some(_), None) => Err(Error::custom(
			ErrorKind::ArchivePasswordRequired,
			format!("an encrypted {format} needs a password"),
		)),
	}
}

fn write_zip(
	port: &WorkerPort,
	mut zip: ZipWriter<ChunkSink<'_>>,
	entries: Vec<ArchiveEntry>,
	method: ZipMethod,
	password: Option<(AesStrength, &ArchivePassword)>,
) -> Result<u64, Error> {
	for entry in entries {
		match entry {
			ArchiveEntry::Dir { path, modified } => {
				zip.add_dir(&path, modified).map_err(failure)?;
			}
			ArchiveEntry::File {
				source,
				path,
				size,
				modified,
			} => {
				let encryption = password.map(|(strength, password)| {
					// a fresh salt per entry, so no two entries share a key
					let mut salt = vec![0u8; strength.salt_len()];
					rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt);
					Encryption {
						password,
						strength,
						salt,
					}
				});
				write_source(port, source, size, |data| {
					zip.add_file(&path, modified, size, method, encryption, data)
				})?;
			}
		}
	}
	zip.finish().map_err(failure)?.finish().map_err(failure)
}

fn write_7z(
	port: &WorkerPort,
	mut writer: SevenZWriter<ChunkSink<'_>>,
	entries: Vec<ArchiveEntry>,
) -> Result<u64, Error> {
	for entry in entries {
		match entry {
			ArchiveEntry::Dir { path, modified } => writer.add_dir(&path, modified),
			ArchiveEntry::File {
				source,
				path,
				size,
				modified,
			} => {
				write_source(port, source, size, |data| {
					writer.add_file(&path, modified, size, data)
				})?;
			}
		}
	}
	let (sink, start) = writer.finish().map_err(failure)?;
	sink.finish_with_head(&start).map_err(failure)
}

fn write_tar<W: Write>(
	port: &WorkerPort,
	tar: &mut tar::Builder<W>,
	entries: Vec<ArchiveEntry>,
) -> Result<(), Error> {
	for entry in entries {
		match entry {
			ArchiveEntry::Dir { path, modified } => {
				let mut header = header(tar::EntryType::Directory, 0o755, 0, modified);
				tar.append_data(&mut header, format!("{path}/"), io::empty())
					.map_err(failure)?;
			}
			ArchiveEntry::File {
				source,
				path,
				size,
				modified,
			} => {
				let mut header = header(tar::EntryType::Regular, 0o644, size, modified);
				write_source(port, source, size, |data| {
					let mut data = Counted {
						inner: data,
						read: 0,
					};
					// a path over 100 bytes goes into a GNU long-name record
					tar.append_data(&mut header, &path, &mut data)?;
					Ok(data.read)
				})?;
			}
		}
	}
	Ok(())
}

/// The exact length of the bare tar [`compress`] writes for `entries`: a header block per entry,
/// a GNU long-name record (a header block and the name with its NUL, in whole blocks) for a
/// stored path over 100 bytes, file data in whole blocks, and the two end-of-archive blocks.
pub(crate) fn tar_size(entries: &[ArchiveEntry]) -> u64 {
	let long_name = |stored: usize| {
		if stored > USTAR_NAME_LEN {
			TAR_BLOCK + (stored as u64 + 1).div_ceil(TAR_BLOCK) * TAR_BLOCK
		} else {
			0
		}
	};
	let members: u64 = entries
		.iter()
		.map(|entry| match entry {
			ArchiveEntry::Dir { path, .. } => TAR_BLOCK + long_name(path.len() + 1),
			ArchiveEntry::File { path, size, .. } => {
				TAR_BLOCK + long_name(path.len()) + size.div_ceil(TAR_BLOCK) * TAR_BLOCK
			}
		})
		.sum();
	members + 2 * TAR_BLOCK
}

fn header(
	kind: tar::EntryType,
	mode: u32,
	size: u64,
	modified: Option<DateTime<Utc>>,
) -> tar::Header {
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(kind);
	header.set_mode(mode);
	header.set_size(size);
	// ustar times are unsigned whole seconds; earlier ones are written as the epoch
	header.set_mtime(modified.map_or(0, |time| time.timestamp().max(0) as u64));
	header
}

/// Writes source `source` as the current file entry with `add`, which returns the bytes it read
/// of it; checks that was all `size` of them, and tells the driver the file is in.
fn write_source(
	port: &WorkerPort,
	source: u32,
	size: u64,
	add: impl FnOnce(&mut ChunkInput<'_>) -> io::Result<u64>,
) -> Result<(), Error> {
	let mut data = ChunkInput::new(port, source, size);
	let read = add(&mut data).map_err(failure)?;
	check_length(read, size)?;
	port.send(WorkerEvent::FileEnd).map_err(failure)
}

/// A source whose data is not the length it was listed with changed while the job ran; the
/// archive cannot hold it, since a tar header states the length before the data.
fn check_length(read: u64, size: u64) -> Result<(), Error> {
	if read != size {
		return Err(Error::custom(
			ErrorKind::FileChangedDuringSync,
			format!("a source file held {read} bytes instead of the {size} it was listed with"),
		));
	}
	Ok(())
}

struct Counted<R> {
	inner: R,
	read: u64,
}

impl<R: Read> Read for Counted<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.read += n as u64;
		Ok(n)
	}
}

/// The error a write or read ended with, as the job reports it.
fn failure(error: io::Error) -> Error {
	let error = match error.downcast::<JobEnded>() {
		Ok(ended) => return ended.into(),
		Err(error) => error,
	};
	if error.kind() == io::ErrorKind::UnexpectedEof {
		return Error::custom(
			ErrorKind::FileChangedDuringSync,
			"a source file ended before its listed size",
		);
	}
	error.into()
}

#[cfg(test)]
mod tests;
