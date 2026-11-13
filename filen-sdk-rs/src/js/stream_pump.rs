use futures::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::{Error, ErrorKind};

/// The most a pump reads from its stream at a time.
const MAX_READ_LEN: usize = 64 * 1024;

/// Pumps a JS `ReadableStream`'s reader into a channel from a task on the CALLING thread, which is
/// where the stream's JS object lives: a `ReadableStream` cannot be moved to the commander.
///
/// Reads at most 64 KiB at a time, and no more than `size_hint` when the caller knows how much
/// there is. `progress` gets the bytes handed over so far after each read.
///
/// A read error arrives in-band as the last item, so the consumer sees it where it sees the bytes;
/// a closed channel only ever means the end of the stream. A consumer that stops reading drops the
/// receiver, and the pump ends with it.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) fn spawn_stream_pump(
	reader: impl AsyncRead + Unpin + 'static,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64) + 'static>,
) -> mpsc::Receiver<Result<Vec<u8>, Error>> {
	let (sender, receiver) = mpsc::channel(10);
	crate::runtime::spawn_local(pump_stream(reader, size_hint, progress, sender));
	receiver
}

/// The body of [`spawn_stream_pump`]'s task.
pub(crate) async fn pump_stream(
	mut reader: impl AsyncRead + Unpin,
	size_hint: Option<u64>,
	progress: Option<impl Fn(u64)>,
	sender: mpsc::Sender<Result<Vec<u8>, Error>>,
) {
	let read_len = size_hint
		.and_then(|size| usize::try_from(size).ok())
		.map_or(MAX_READ_LEN, |size| size.min(MAX_READ_LEN));
	let mut buffer = vec![0u8; read_len];
	let mut written = 0u64;
	loop {
		match reader.read(&mut buffer).await {
			Ok(0) => return, // EOF
			Ok(n) => {
				// a closed channel: the consumer has ended, with a result of its own
				if sender.send(Ok(buffer[..n].to_vec())).await.is_err() {
					return;
				}
				if let Some(progress) = &progress {
					written += n as u64;
					progress(written);
				}
			}
			Err(e) => {
				let _ = sender
					.send(Err(Error::custom(
						ErrorKind::IO,
						format!("error reading from stream: {e:?}"),
					)))
					.await;
				return;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, io};

	use futures::{TryStreamExt, io::Cursor, stream};

	use super::*;

	/// Every item `pump_stream` sends over `reader`, once it has ended.
	async fn pump(
		reader: impl AsyncRead + Unpin,
		size_hint: Option<u64>,
		progress: Option<impl Fn(u64)>,
	) -> Vec<Result<Vec<u8>, Error>> {
		let (sender, mut receiver) = mpsc::channel(16);
		pump_stream(reader, size_hint, progress, sender).await;
		let mut items = Vec::new();
		while let Some(item) = receiver.recv().await {
			items.push(item);
		}
		items
	}

	fn chunks(items: Vec<Result<Vec<u8>, Error>>) -> Vec<Vec<u8>> {
		items.into_iter().map(Result::unwrap).collect()
	}

	#[tokio::test]
	async fn a_pump_reads_at_most_the_size_hint_at_a_time() {
		let progress = RefCell::new(Vec::new());
		let items = pump(
			Cursor::new(b"abcdefghij".to_vec()),
			Some(4),
			Some(|written| progress.borrow_mut().push(written)),
		)
		.await;
		assert_eq!(
			chunks(items),
			[b"abcd".to_vec(), b"efgh".to_vec(), b"ij".to_vec()]
		);
		assert_eq!(progress.into_inner(), [4, 8, 10]);
	}

	#[tokio::test]
	async fn a_pump_without_a_size_hint_reads_up_to_64_kib_at_a_time() {
		let items = pump(
			Cursor::new(vec![7u8; MAX_READ_LEN + 1]),
			None,
			None::<fn(u64)>,
		)
		.await;
		let lens = chunks(items).iter().map(Vec::len).collect::<Vec<_>>();
		assert_eq!(lens, [MAX_READ_LEN, 1]);
	}

	#[tokio::test]
	async fn a_pump_sends_a_read_error_as_its_last_item() {
		let reader = stream::iter([
			Ok(b"partial".to_vec()),
			Err(io::Error::other("broken")),
			Ok(b"never read".to_vec()),
		])
		.into_async_read();
		let mut items = pump(reader, None, None::<fn(u64)>).await;

		let error = items.pop().unwrap().unwrap_err();
		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: Custom { kind: Other, error: \"broken\" }"
		);
		assert_eq!(chunks(items), [b"partial".to_vec()]);
	}

	#[tokio::test]
	async fn a_pump_stops_once_its_consumer_is_gone() {
		let (sender, receiver) = mpsc::channel(16);
		drop(receiver);
		let progress = RefCell::new(Vec::new());
		let mut reader = Cursor::new(b"abcdefgh".to_vec());

		pump_stream(
			&mut reader,
			Some(4),
			Some(|written| progress.borrow_mut().push(written)),
			sender,
		)
		.await;

		assert_eq!(reader.position(), 4);
		assert!(progress.into_inner().is_empty());
	}
}
