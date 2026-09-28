//! The BLAKE3 hash of an archive whose first chunk is written last (a 7z's start header points
//! at its end): every later chunk is hashed as it streams by, as a subtree of BLAKE3's tree, and
//! the first chunk's subtree is merged in once it arrives. Nothing but chaining values is kept.

use blake3::hazmat::{
	ChainingValue, HasherExt, Mode, merge_subtrees_non_root, merge_subtrees_root,
};

use crate::consts::CHUNK_SIZE_U64;

// every chunk but the last is a whole BLAKE3 subtree: a power of two of its 1 KiB chunks
const _: () = assert!(CHUNK_SIZE_U64.is_power_of_two() && CHUNK_SIZE_U64 >= 1024);

/// Chunks `1..` arrive in order, then the head (chunk 0); each chunk but the last is exactly
/// [`CHUNK_SIZE_U64`] bytes.
pub(crate) struct HeadLastHasher {
	/// Chunks taken after the head.
	chunks: u64,
	/// The newest chunk's chaining value, not merged yet: whether it is the last one decides
	/// how it merges.
	pending: Option<ChainingValue>,
	/// Complete subtrees right of the head's, oldest first, as BLAKE3's own hasher stacks them.
	stack: Vec<ChainingValue>,
	/// The subtrees merged onto the head's so far, in order: the head's subtree grows by one
	/// of these each time the stack would merge into it.
	head_siblings: Vec<ChainingValue>,
}

impl HeadLastHasher {
	pub(crate) fn new() -> Self {
		Self {
			chunks: 0,
			pending: None,
			stack: Vec::new(),
			head_siblings: Vec::new(),
		}
	}

	/// Chunk `chunks + 1` of the archive.
	pub(crate) fn update(&mut self, data: &[u8]) {
		if let Some(previous) = self.pending.take() {
			self.push(previous);
		}
		self.chunks += 1;
		let mut hasher = blake3::Hasher::new();
		hasher.set_input_offset(self.chunks * CHUNK_SIZE_U64);
		hasher.update(data);
		self.pending = Some(hasher.finalize_non_root());
	}

	/// Pushes the chaining value of a chunk known not to be the last, merging complete
	/// subtrees as BLAKE3 does: after chunk count `n`, one merge per trailing zero bit of `n`.
	fn push(&mut self, mut cv: ChainingValue) {
		// the head counts as the first chunk
		let mut total = self.chunks + 1;
		while total & 1 == 0 {
			match self.stack.pop() {
				Some(left) => cv = merge_subtrees_non_root(&left, &cv, Mode::Hash),
				None => {
					self.head_siblings.push(cv);
					return;
				}
			}
			total >>= 1;
		}
		self.stack.push(cv);
	}

	/// The hash of the whole archive, given its head.
	pub(crate) fn finalize(self, head: &[u8]) -> blake3::Hash {
		let Some(last) = self.pending else {
			// the head is the whole archive
			return blake3::hash(head);
		};
		let mut hasher = blake3::Hasher::new();
		hasher.set_input_offset(0);
		hasher.update(head);
		let left = self
			.head_siblings
			.iter()
			.fold(hasher.finalize_non_root(), |left, right| {
				merge_subtrees_non_root(&left, right, Mode::Hash)
			});
		let right = self.stack.iter().rev().fold(last, |right, left| {
			merge_subtrees_non_root(left, &right, Mode::Hash)
		});
		merge_subtrees_root(&left, &right, Mode::Hash)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::archive::test_support::pattern;

	fn hash_head_last(data: &[u8]) -> blake3::Hash {
		let chunk = CHUNK_SIZE_U64 as usize;
		let mut hasher = HeadLastHasher::new();
		for piece in data.chunks(chunk).skip(1) {
			hasher.update(piece);
		}
		hasher.finalize(&data[..data.len().min(chunk)])
	}

	#[test]
	fn matches_hashing_in_order_at_every_shape() {
		let chunk = CHUNK_SIZE_U64 as usize;
		let data = pattern(9 * chunk + 1, 0);
		let mut lens = vec![0, 1, 1024, 1025, chunk - 1, chunk, chunk + 1, chunk + 1024];
		for chunks in 2..=9 {
			lens.extend([chunks * chunk - 1, chunks * chunk, chunks * chunk + 1]);
		}
		for len in lens {
			assert_eq!(
				hash_head_last(&data[..len]),
				blake3::hash(&data[..len]),
				"{len} bytes"
			);
		}
	}
}
