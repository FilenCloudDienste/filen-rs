use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExifTimes {
	pub created: Option<DateTime<Utc>>,
	pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExifMediaKind {
	Image,
	Track,
}

pub(crate) fn mime_to_exif_kind(mime: &str) -> Option<ExifMediaKind> {
	if mime.starts_with("image/") {
		Some(ExifMediaKind::Image)
	} else if mime.starts_with("video/") || mime.starts_with("audio/") {
		Some(ExifMediaKind::Track)
	} else {
		None
	}
}

pub(crate) use imp::ExifTeeState;

/// Resolve the final `(created, modified)` timestamps that will be encoded into
/// the uploaded file's metadata, given the caller-supplied builder values and
/// the EXIF parse result.
///
/// Override matrix per axis (created, modified handled independently):
/// - if `exif_value` is Some AND `override_user_times` is true → exif wins
/// - else if `user_value` is Some → user wins
/// - else if `exif_value` is Some → exif fills the gap
/// - else → fallback (caller's already-resolved BaseFile value)
pub(crate) fn resolve_final_times(
	user_created: Option<DateTime<Utc>>,
	user_modified: Option<DateTime<Utc>>,
	exif: ExifTimes,
	override_user_times: bool,
	fallback_time: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
	let created = match (user_created, exif.created, override_user_times) {
		(_, Some(e), true) => e,
		(Some(u), _, _) => u,
		(None, Some(e), _) => e,
		(None, None, _) => fallback_time,
	};
	let modified = match (user_modified, exif.modified, override_user_times) {
		(_, Some(e), true) => e,
		(Some(u), _, _) => u,
		(None, Some(e), _) => e,
		(None, None, _) => fallback_time,
	};
	(created, modified)
}

mod imp {
	use std::pin::Pin;
	use std::task::{Context, Poll};

	use bytes::Bytes;
	use chrono::{DateTime, Utc};
	use futures::FutureExt;
	use nom_exif::{AsyncMediaSource, EntryValue, Exif, ExifTag, MediaParser, TrackInfoTag};
	use tokio::io::{AsyncRead, ReadBuf};
	use tokio::sync::mpsc;
	use tokio_util::sync::PollSender;

	use crate::runtime::SpawnTaskHandle;
	use crate::{Error, ErrorKind};

	use super::{ExifMediaKind, ExifTimes, resolve_final_times};
	use chrono::SubsecRound;

	// Cross-target spawn handle for the parser task. On native tokio, we use
	// tokio::spawn (multi-threaded runtime, Send required). On wasm32 there's
	// no tokio::spawn — wasm_bindgen_futures::spawn_local + a oneshot channel
	// stands in. Both end up implementing Future<Output = Result<ExifTimes, _>>
	// so the poll_finalize logic is identical.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	type ParseHandle = tokio::task::JoinHandle<ExifTimes>;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	type ParseHandle = tokio::sync::oneshot::Receiver<ExifTimes>;

	pub(crate) struct ExifChannelReader {
		rx: mpsc::Receiver<Bytes>,
		leftover: Bytes,
	}

	impl ExifChannelReader {
		pub(crate) fn new(rx: mpsc::Receiver<Bytes>) -> Self {
			Self {
				rx,
				leftover: Bytes::new(),
			}
		}
	}

	impl AsyncRead for ExifChannelReader {
		fn poll_read(
			mut self: Pin<&mut Self>,
			cx: &mut Context<'_>,
			buf: &mut ReadBuf<'_>,
		) -> Poll<std::io::Result<()>> {
			if self.leftover.is_empty() {
				match self.rx.poll_recv(cx) {
					Poll::Ready(Some(chunk)) => self.leftover = chunk,
					Poll::Ready(None) => return Poll::Ready(Ok(())),
					Poll::Pending => return Poll::Pending,
				}
			}
			let take = self.leftover.len().min(buf.remaining());
			let bytes = self.leftover.split_to(take);
			buf.put_slice(&bytes);
			Poll::Ready(Ok(()))
		}
	}

	fn entry_value_to_utc(v: &EntryValue) -> Option<chrono::DateTime<chrono::Utc>> {
		match v {
			EntryValue::DateTime(dt) => Some(dt.with_timezone(&chrono::Utc).round_subsecs(3)),
			EntryValue::NaiveDateTime(ndt) => Some(ndt.and_utc().round_subsecs(3)),
			_ => None,
		}
	}

	/// EXIF teeing state owned by `FileWriter` when EXIF parsing is enabled.
	///
	/// `poll_tee` is called by the writer's `poll_write` per chunk; it forwards
	/// bytes into the parser's mpsc channel with backpressure when the parser
	/// falls behind. `poll_finalize` is called from `poll_close` to drain any
	/// pending bytes, signal EOF, await the parser, and produce the final
	/// `(created, modified)` to bake into the file's encrypted metadata.
	pub(crate) struct ExifTeeState {
		tx: PollSender<Bytes>,
		parse_handle: SpawnTaskHandle<Result<ExifTimes, Error>>,
		pending: Option<Bytes>,
		override_with_exif: bool,
		user_created: Option<DateTime<Utc>>,
		user_modified: Option<DateTime<Utc>>,
		close_state: TeeCloseState,
		fallback_time: DateTime<Utc>,
	}

	#[derive(Debug, PartialEq, Eq)]
	enum TeeCloseState {
		Open,
		DrainPending,
		AwaitParser,
		Done,
	}

	impl ExifTeeState {
		pub(crate) fn new(
			kind: ExifMediaKind,
			override_with_exif: bool,
			user_created: Option<DateTime<Utc>>,
			user_modified: Option<DateTime<Utc>>,
			fallback_time: DateTime<Utc>,
		) -> Self {
			let (tx, rx) = mpsc::channel::<Bytes>(4);
			let parse_handle = crate::runtime::spawn_task_maybe_send(parse_exif_stream(rx, kind));
			Self {
				tx: PollSender::new(tx),
				parse_handle,
				pending: None,
				override_with_exif,
				user_created,
				user_modified,
				close_state: TeeCloseState::Open,
				fallback_time,
			}
		}

		/// Forward bytes to the parser channel. Returns `Pending` only if the
		/// parser channel is full AND we have nothing else to do — applying
		/// backpressure to the writer's read loop.
		pub(crate) fn poll_tee(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<()> {
			// Drain any leftover pending chunk first.
			if let Some(pending) = self.pending.take() {
				match self.tx.poll_reserve(cx) {
					Poll::Ready(Ok(())) => {
						let _ = self.tx.send_item(pending);
					}
					Poll::Ready(Err(_)) => {
						// channel closed (parser finished/aborted)
						return Poll::Ready(());
					}
					Poll::Pending => {
						self.pending = Some(pending);
						return Poll::Pending;
					}
				}
			}
			if buf.is_empty() {
				return Poll::Ready(());
			}
			match self.tx.poll_reserve(cx) {
				Poll::Ready(Ok(())) => {
					let bytes = Bytes::copy_from_slice(buf);
					let _ = self.tx.send_item(bytes);
				}
				Poll::Ready(Err(_)) => {
					// parser is gone — drop bytes silently
				}
				Poll::Pending => {
					let bytes = Bytes::copy_from_slice(buf);
					self.pending = Some(bytes);
					return Poll::Pending;
				}
			}
			Poll::Ready(())
		}

		/// Drain pending, signal EOF to the parser, await it, and return the
		/// resolved `(created, modified)` to install into the writer's metadata.
		#[allow(clippy::type_complexity)]
		pub(crate) fn poll_finalize(
			&mut self,
			cx: &mut Context<'_>,
		) -> Poll<Result<(DateTime<Utc>, DateTime<Utc>), Error>> {
			loop {
				match self.close_state {
					TeeCloseState::Open => {
						self.close_state = TeeCloseState::DrainPending;
					}
					TeeCloseState::DrainPending => {
						if let Some(pending) = self.pending.take() {
							match self.tx.poll_reserve(cx) {
								Poll::Ready(Ok(())) => {
									let _ = self.tx.send_item(pending);
								}
								Poll::Ready(Err(_)) => {
									// closed — fall through
								}
								Poll::Pending => {
									self.pending = Some(pending);
									return Poll::Pending;
								}
							}
						}
						// All pending drained; drop the sender to signal EOF.
						self.tx.close();
						self.close_state = TeeCloseState::AwaitParser;
					}
					TeeCloseState::AwaitParser => match self.parse_handle.poll_unpin(cx) {
						Poll::Ready(exif) => {
							let exif = match exif {
								Ok(exif) => exif,
								Err(e) => {
									return Poll::Ready(Err(Error::custom_with_source(
										ErrorKind::Internal,
										e,
										Some("failed to parse EXIF from media stream"),
									)));
								}
							};

							let (c, m) = resolve_final_times(
								self.user_created,
								self.user_modified,
								exif,
								self.override_with_exif,
								self.fallback_time,
							);
							self.close_state = TeeCloseState::Done;
							return Poll::Ready(Ok((c, m)));
						}
						Poll::Pending => return Poll::Pending,
					},
					TeeCloseState::Done => {
						panic!("poll_finalize called after completion");
					}
				}
			}
		}
	}

	pub(crate) async fn parse_exif_stream(
		rx: mpsc::Receiver<Bytes>,
		kind: ExifMediaKind,
	) -> Result<ExifTimes, Error> {
		let reader = ExifChannelReader::new(rx);
		let ms = AsyncMediaSource::unseekable(reader).await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("failed to build AsyncMediaSource"),
			)
		})?;
		let mut parser = MediaParser::new();
		match kind {
			ExifMediaKind::Image => {
				let iter = parser.parse_exif_async(ms).await.map_err(|e| {
					Error::custom_with_source(
						ErrorKind::Internal,
						e,
						Some("failed to parse EXIF from image stream"),
					)
				})?;
				let exif: Exif = iter.into();
				Ok(ExifTimes {
					created: exif
						.get(ExifTag::DateTimeOriginal)
						.and_then(entry_value_to_utc)
						.or_else(|| exif.get(ExifTag::CreateDate).and_then(entry_value_to_utc)),
					modified: exif.get(ExifTag::ModifyDate).and_then(entry_value_to_utc),
				})
			}
			ExifMediaKind::Track => {
				let info = parser.parse_track_async(ms).await.map_err(|e| {
					Error::custom_with_source(
						ErrorKind::Internal,
						e,
						Some("failed to parse track info from media stream"),
					)
				})?;
				Ok(ExifTimes {
					created: info
						.get(TrackInfoTag::CreateDate)
						.and_then(entry_value_to_utc),
					modified: None,
				})
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use chrono::TimeZone;

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test]
	async fn channel_reader_yields_bytes_then_eof() {
		use bytes::Bytes;
		use std::pin::Pin;
		use tokio::io::AsyncReadExt;
		use tokio::sync::mpsc;

		let (tx, rx) = mpsc::channel::<Bytes>(4);
		let mut reader = imp::ExifChannelReader::new(rx);

		// Feed two chunks then drop tx to signal EOF.
		tokio::spawn(async move {
			tx.send(Bytes::from_static(b"hello, ")).await.unwrap();
			tx.send(Bytes::from_static(b"world!")).await.unwrap();
			drop(tx);
		});

		let mut buf = Vec::new();
		Pin::new(&mut reader).read_to_end(&mut buf).await.unwrap();
		assert_eq!(buf, b"hello, world!");
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test]
	async fn parse_exif_stream_returns_err_on_garbage() {
		use bytes::Bytes;
		use tokio::sync::mpsc;

		let (tx, rx) = mpsc::channel::<Bytes>(4);
		tokio::spawn(async move {
			tx.send(Bytes::from_static(b"not a real image at all"))
				.await
				.ok();
			drop(tx);
		});
		imp::parse_exif_stream(rx, ExifMediaKind::Image)
			.await
			.unwrap_err();
	}

	#[test]
	fn resolve_user_wins_when_no_override() {
		let user_t = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
		let exif_t = Utc.with_ymd_and_hms(2021, 6, 1, 12, 0, 0).unwrap();
		let exif = ExifTimes {
			created: Some(exif_t),
			modified: Some(exif_t),
		};
		let fb = Utc::now();
		let (c, m) = resolve_final_times(Some(user_t), Some(user_t), exif, false, fb);
		assert_eq!(c, user_t);
		assert_eq!(m, user_t);
	}

	#[test]
	fn resolve_exif_overrides_user_when_flag_set() {
		let user_t = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
		let exif_t = Utc.with_ymd_and_hms(2021, 6, 1, 12, 0, 0).unwrap();
		let exif = ExifTimes {
			created: Some(exif_t),
			modified: Some(exif_t),
		};
		let (c, m) = resolve_final_times(Some(user_t), Some(user_t), exif, true, Utc::now());
		assert_eq!(c, exif_t);
		assert_eq!(m, exif_t);
	}

	#[test]
	fn resolve_exif_fills_unset_even_without_override() {
		let exif_t = Utc.with_ymd_and_hms(2021, 6, 1, 12, 0, 0).unwrap();
		let exif = ExifTimes {
			created: Some(exif_t),
			modified: None,
		};
		let fb = Utc::now();
		let (c, m) = resolve_final_times(None, None, exif, false, fb);
		assert_eq!(c, exif_t);
		assert_eq!(m, fb); // exif modified was None, falls back
	}

	#[test]
	fn resolve_independent_axes() {
		// created from exif (override), modified from user (no exif modified)
		let user_m = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
		let exif_c = Utc.with_ymd_and_hms(2021, 6, 1, 12, 0, 0).unwrap();
		let exif = ExifTimes {
			created: Some(exif_c),
			modified: None,
		};

		let (c, m) = resolve_final_times(None, Some(user_m), exif, true, Utc::now());
		assert_eq!(c, exif_c);
		assert_eq!(m, user_m);
	}

	#[test]
	fn mime_image_is_image() {
		assert_eq!(mime_to_exif_kind("image/jpeg"), Some(ExifMediaKind::Image));
		assert_eq!(mime_to_exif_kind("image/heic"), Some(ExifMediaKind::Image));
		assert_eq!(mime_to_exif_kind("image/png"), Some(ExifMediaKind::Image));
	}

	#[test]
	fn mime_video_audio_is_track() {
		assert_eq!(mime_to_exif_kind("video/mp4"), Some(ExifMediaKind::Track));
		assert_eq!(
			mime_to_exif_kind("video/quicktime"),
			Some(ExifMediaKind::Track)
		);
		assert_eq!(mime_to_exif_kind("audio/mpeg"), Some(ExifMediaKind::Track));
	}

	#[test]
	fn mime_other_is_none() {
		assert_eq!(mime_to_exif_kind("text/plain"), None);
		assert_eq!(mime_to_exif_kind("application/pdf"), None);
		assert_eq!(mime_to_exif_kind("application/octet-stream"), None);
	}
}
