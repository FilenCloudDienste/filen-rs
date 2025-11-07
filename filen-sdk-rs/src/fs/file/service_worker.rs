use futures::{AsyncWrite, future::BoxFuture};
pub(crate) struct StreamWriter {
	sender: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
	// change to stack based future once https://github.com/rust-lang/rust/issues/63063 is stabilized
	flush_fut: Option<BoxFuture<'static, std::io::Result<()>>>,
	current_chunk: Option<Vec<u8>>,
}

impl StreamWriter {
	pub fn new(sender: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
		Self {
			sender: Some(sender),
			current_chunk: None,
			flush_fut: None,
		}
	}
}

pub(crate) const MAX_BUFFER_SIZE_BEFORE_FLUSH: usize = 64 * 1024; // 64 KB

async fn make_flush_fut(
	sender: tokio::sync::mpsc::Sender<Vec<u8>>,
	chunk: Vec<u8>,
) -> std::io::Result<()> {
	sender.send(chunk).await.map_err(std::io::Error::other)
}

impl StreamWriter {
	fn get_or_make_flush_fut(
		&mut self,
	) -> Result<Option<&mut BoxFuture<'static, Result<(), std::io::Error>>>, std::io::Error> {
		let flush_fut = match self.flush_fut.take() {
			Some(future) => future,
			None => {
				let Some(ref sender) = self.sender else {
					return Err(std::io::Error::new(
						std::io::ErrorKind::BrokenPipe,
						"stream already closed when trying to flush",
					));
				};
				if let Some(chunk) = self.current_chunk.take() {
					Box::pin(make_flush_fut(sender.clone(), chunk))
				} else {
					return Ok(None);
				}
			}
		};
		self.flush_fut.replace(flush_fut);
		Ok(Some(self.flush_fut.as_mut().unwrap()))
	}
}

impl AsyncWrite for StreamWriter {
	fn poll_write(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		let this = self.get_mut();

		if let Some(future) = this.flush_fut.as_mut() {
			match future.as_mut().poll(cx) {
				std::task::Poll::Ready(res) => {
					this.flush_fut.take();
					res?;
				}
				std::task::Poll::Pending => {
					return std::task::Poll::Pending;
				}
			}
		}

		let Some(sender) = &this.sender else {
			return std::task::Poll::Ready(Err(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"stream already closed when trying to write",
			)));
		};

		let len = buf.len();
		let current_chunk = match this.current_chunk.take() {
			Some(mut chunk) => {
				chunk.extend(buf);
				chunk
			}
			None => buf.to_vec(),
		};

		if current_chunk.len() >= MAX_BUFFER_SIZE_BEFORE_FLUSH {
			this.flush_fut
				.replace(Box::pin(make_flush_fut(sender.clone(), current_chunk)));
		} else {
			this.current_chunk = Some(current_chunk);
		}
		std::task::Poll::Ready(Ok(len))
	}

	fn poll_flush(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		let mut this = self.as_mut();

		let flush_fut = match this.get_or_make_flush_fut() {
			Ok(Some(fut)) => fut,
			Ok(None) => return std::task::Poll::Ready(Ok(())),
			Err(e) => return std::task::Poll::Ready(Err(e)),
		};

		match flush_fut.as_mut().poll(cx) {
			std::task::Poll::Ready(res) => {
				this.flush_fut.take();
				std::task::Poll::Ready(res)
			}
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}

	fn poll_close(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		let this = self.get_mut();

		let maybe_flush_fut = match this.get_or_make_flush_fut() {
			Ok(maybe_fut) => maybe_fut,
			Err(e) => return std::task::Poll::Ready(Err(e)),
		};
		if let Some(flush_fut) = maybe_flush_fut {
			match flush_fut.as_mut().poll(cx) {
				std::task::Poll::Ready(res) => {
					this.flush_fut.take();
					res?;
				}
				std::task::Poll::Pending => {
					return std::task::Poll::Pending;
				}
			}
		}

		if this.sender.take().is_some() {
			std::task::Poll::Ready(Ok(()))
		} else {
			std::task::Poll::Ready(Err(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"stream already closed when trying to close",
			)))
		}
	}
}
