//! The codec side of compressing: runs on the codec worker, reads each source file through the
//! driver chunk by chunk, and writes the archive into a [`ChunkSink`] the driver uploads from.

use std::io::{self, Read, Write};

use chrono::{DateTime, Utc};

use crate::{
	Error, ErrorKind,
	fs::archive::{
		encode::{StreamEncoder, open_encoder},
		password::ArchivePassword,
		sevenz::write::SevenZWriter,
		tar_iter::{TAR_BLOCK, USTAR_NAME_LEN},
		worker::{ChunkInput, ChunkSink, HeadSink, JobEnded, WorkerEvent, WorkerPort},
		zip::{
			crypto::AesStrength,
			write::{Encryption, ZipMethod, ZipWriter},
		},
	},
};

use super::{CheckedArchive, Compression};

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

/// What the codec writes.
#[derive(Debug)]
pub(crate) enum CompressJob {
	/// A tar, zip or 7z of `entries`.
	Archive {
		format: CheckedArchive,
		entries: Vec<ArchiveEntry>,
	},
	/// Input `source`, `size` bytes, compressed on its own.
	Single {
		compression: Compression,
		source: u32,
		size: u64,
	},
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
	let (format, entries) = match job {
		CompressJob::Archive { format, entries } => (format, entries),
		CompressJob::Single {
			compression,
			source,
			size,
		} => {
			let mut encoder: Box<dyn StreamEncoder<ChunkSink<'_>>> =
				open_encoder(compression, ChunkSink::new(port))?;
			write_source(port, source, size, |data| io::copy(data, &mut encoder))?;
			return encoder.finish().map_err(failure)?.finish().map_err(failure);
		}
	};
	match format {
		CheckedArchive::Tar { compression: None } => {
			let mut tar = tar::Builder::new(ChunkSink::new(port));
			write_tar(port, &mut tar, entries)?;
			// writes the two end-of-archive blocks
			tar.into_inner().map_err(failure)?.finish().map_err(failure)
		}
		CheckedArchive::Tar {
			compression: Some(compression),
		} => {
			let mut tar = tar::Builder::new(open_encoder(compression, ChunkSink::new(port))?);
			write_tar(port, &mut tar, entries)?;
			let encoder = tar.into_inner().map_err(failure)?;
			encoder.finish().map_err(failure)?.finish().map_err(failure)
		}
		CheckedArchive::Zip { method, encryption } => {
			let zip = zip_writer(ChunkSink::new(port));
			let encryption = encryption
				.as_ref()
				.map(|(strength, password)| (*strength, password));
			write_zip(port, zip, entries, method, encryption)
		}
		CheckedArchive::SevenZ {
			method,
			solid,
			encryption,
		} => {
			let encryption = encryption
				.as_ref()
				.map(|(what, password)| (*what, password));
			// a 7z's start points at its header, so its first chunk is sent last
			let writer = SevenZWriter::new(HeadSink::new(port), method, solid, encryption)
				.map_err(failure)?;
			write_7z(port, writer, entries)
		}
	}
}

fn write_zip(
	port: &WorkerPort,
	mut zip: ZipWriter<ChunkSink<'_>>,
	entries: Vec<ArchiveEntry>,
	method: ZipMethod,
	encryption: Option<(AesStrength, &ArchivePassword)>,
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
				let encryption =
					encryption.map(|(strength, password)| Encryption { password, strength });
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
	mut writer: SevenZWriter<HeadSink<'_>>,
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
