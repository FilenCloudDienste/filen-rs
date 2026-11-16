//! The input side of the protocol's framing: lines of at most [`MAX_LINE_BYTES`].

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::helper::protocol::MAX_LINE_BYTES;

/// One line read from the parent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
	/// A line, its newline and a trailing `\r` stripped; never empty.
	Line(String),
	/// A line longer than [`MAX_LINE_BYTES`], read and discarded up to its newline.
	TooLong,
	/// A line that is not UTF-8.
	InvalidUtf8,
	/// The input ended.
	Eof,
}

/// Reads the next non-empty line from `input` into `buf` (cleared first), keeping at most
/// [`MAX_LINE_BYTES`] of it: a longer line is read to its end and discarded. A last line without
/// a newline counts as a line.
pub(crate) async fn next_frame<R: AsyncBufRead + Unpin>(
	input: &mut R,
	buf: &mut Vec<u8>,
) -> std::io::Result<Frame> {
	loop {
		buf.clear();
		let mut too_long = false;
		let mut ended = false;
		loop {
			let available = input.fill_buf().await?;
			if available.is_empty() {
				ended = true;
				break;
			}
			let (taken, newline) = match available.iter().position(|&b| b == b'\n') {
				Some(at) => (at, true),
				None => (available.len(), false),
			};
			if !too_long {
				let room = MAX_LINE_BYTES.saturating_sub(buf.len());
				if taken > room {
					too_long = true;
					buf.clear();
				} else if let Some(part) = available.get(..taken) {
					buf.extend_from_slice(part);
				}
			}
			input.consume(if newline { taken + 1 } else { taken });
			if newline {
				break;
			}
		}
		if too_long {
			return Ok(Frame::TooLong);
		}
		if buf.last() == Some(&b'\r') {
			buf.pop();
		}
		if buf.is_empty() {
			if ended {
				return Ok(Frame::Eof);
			}
			continue;
		}
		return Ok(match String::from_utf8(std::mem::take(buf)) {
			Ok(line) => Frame::Line(line),
			Err(_) => Frame::InvalidUtf8,
		});
	}
}

#[cfg(test)]
mod tests {
	use tokio::io::BufReader;

	use super::*;

	/// The frames `input` reads as, up to its end.
	async fn frames(input: &[u8]) -> Vec<Frame> {
		let mut reader = BufReader::with_capacity(16, input);
		let mut buf = Vec::new();
		let mut frames = Vec::new();
		loop {
			let frame = next_frame(&mut reader, &mut buf).await.unwrap();
			let end = frame == Frame::Eof;
			frames.push(frame);
			if end {
				return frames;
			}
		}
	}

	#[tokio::test]
	async fn lines_are_split_and_empty_ones_and_carriage_returns_dropped() {
		assert_eq!(
			frames(b"a\r\n\n\r\nbb\nlast").await,
			[
				Frame::Line("a".to_owned()),
				Frame::Line("bb".to_owned()),
				Frame::Line("last".to_owned()),
				Frame::Eof,
			]
		);
	}

	#[tokio::test]
	async fn a_line_at_the_cap_is_kept_and_one_byte_more_discarded_to_its_newline() {
		let mut input = vec![b'a'; MAX_LINE_BYTES];
		input.push(b'\n');
		input.extend(vec![b'b'; MAX_LINE_BYTES + 1]);
		input.extend_from_slice(b"\nnext\n");
		let frames = frames(&input).await;
		assert!(matches!(&frames[0], Frame::Line(line) if line.len() == MAX_LINE_BYTES));
		assert_eq!(
			frames[1..],
			[Frame::TooLong, Frame::Line("next".to_owned()), Frame::Eof]
		);
	}

	#[tokio::test]
	async fn a_line_that_is_not_utf8_is_its_own_frame() {
		assert_eq!(
			frames(b"\xff\xfe\nok\n").await,
			[Frame::InvalidUtf8, Frame::Line("ok".to_owned()), Frame::Eof]
		);
	}
}
