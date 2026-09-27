//! gzip and bzip2: concatenated members (as `pigz`, `pbzip2` or `cat a.gz b.gz` write them),
//! each decoded by its crate's single-member `bufread` decoder. That decoder stops at its
//! member's trailer, having checked it, and leaves what follows in our buffer, which is how the
//! next member, or data that belongs to none, is found.

use std::{
	io::{self, Read},
	mem,
};

use bzip2::bufread::BzDecoder;
use flate2::bufread::GzDecoder;

use super::{Budget, CodecError, Describe, Input, StreamCheck, StreamDecoder, StreamEnd};

/// miniz_oxide's inflate state (a 32 KiB window and its tables) plus the header fields flate2
/// keeps, which it caps at 64 KiB each.
const GZIP_DECODER_BYTES: u64 = 64 * 1024 + 3 * 64 * 1024;

/// bzip2's documented decompression memory at block size 9 without the `small` mode: 100 kB +
/// 4 × 900 kB. Charged whatever a member's header says, since any member may use 9.
const BZIP2_DECODER_BYTES: u64 = 3_700_000;

pub(super) fn gzip<R: Read>(
	input: Input<R>,
	budget: Budget,
) -> Result<Members<R, GzDecoder<Input<R>>>, CodecError> {
	budget.charge(GZIP_DECODER_BYTES)?;
	Ok(Members::new(input))
}

pub(super) fn bzip2<R: Read>(
	input: Input<R>,
	budget: Budget,
) -> Result<Members<R, BzDecoder<Input<R>>>, CodecError> {
	budget.charge(BZIP2_DECODER_BYTES)?;
	Ok(Members::new(input))
}

/// A crate decoder for one member.
pub(super) trait Member<R>: Read + Sized {
	/// Bytes [`Member::starts`] looks at.
	const MAGIC_LEN: usize;
	const NOT_A_STREAM: &'static str;
	const INVALID_DATA: &'static str;

	fn starts(head: &[u8]) -> bool;
	fn open(input: Input<R>) -> Self;
	fn into_input(self) -> Input<R>;
}

impl<R: Read> Member<R> for GzDecoder<Input<R>> {
	const MAGIC_LEN: usize = 2;
	const NOT_A_STREAM: &'static str = "not a gzip stream";
	const INVALID_DATA: &'static str = "invalid gzip data";

	fn starts(head: &[u8]) -> bool {
		head.starts_with(&[0x1F, 0x8B])
	}

	fn open(input: Input<R>) -> Self {
		Self::new(input)
	}

	fn into_input(self) -> Input<R> {
		self.into_inner()
	}
}

impl<R: Read> Member<R> for BzDecoder<Input<R>> {
	const MAGIC_LEN: usize = 4;
	const NOT_A_STREAM: &'static str = "not a bzip2 stream";
	const INVALID_DATA: &'static str = "invalid bzip2 data";

	fn starts(head: &[u8]) -> bool {
		head.starts_with(b"BZh") && head.get(3).is_some_and(|b| (b'1'..=b'9').contains(b))
	}

	fn open(input: Input<R>) -> Self {
		Self::new(input)
	}

	fn into_input(self) -> Input<R> {
		self.into_inner()
	}
}

enum State<R, D> {
	/// Looking for the next member's magic.
	Between(Input<R>),
	Member(D),
	Done,
	/// An error ended decoding.
	Failed,
}

pub(super) struct Members<R, D> {
	state: State<R, D>,
	members: u64,
	end: Option<StreamEnd>,
}

impl<R: Read, D: Member<R>> Members<R, D> {
	fn new(input: Input<R>) -> Self {
		Self {
			state: State::Between(input),
			members: 0,
			end: None,
		}
	}
}

impl<R: Read, D: Member<R>> Read for Members<R, D> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		loop {
			// an error leaves the state `Failed`
			self.state = match mem::replace(&mut self.state, State::Failed) {
				State::Member(mut member) => {
					let read = member.read(buf)?;
					if read > 0 {
						self.state = State::Member(member);
						return Ok(read);
					}
					State::Between(member.into_input())
				}
				State::Between(mut input) => {
					if D::starts(input.fill_to(D::MAGIC_LEN)?) {
						self.members += 1;
						State::Member(D::open(input))
					} else if self.members == 0 {
						return Err(CodecError::Corrupt(D::NOT_A_STREAM).into());
					} else {
						self.end = Some(StreamEnd {
							check: StreamCheck::Verified,
							unaccounted_bytes: input.drain_trailing(&[])?,
						});
						State::Done
					}
				}
				State::Done => {
					self.state = State::Done;
					return Ok(0);
				}
				State::Failed => return Err(CodecError::Corrupt(D::INVALID_DATA).into()),
			};
		}
	}
}

impl<R: Read, D: Member<R>> StreamDecoder for Members<R, D> {
	fn end(&self) -> Option<StreamEnd> {
		self.end
	}
}

impl<R, D: Member<R>> Describe for Members<R, D> {
	const INVALID: &'static str = D::INVALID_DATA;
}
