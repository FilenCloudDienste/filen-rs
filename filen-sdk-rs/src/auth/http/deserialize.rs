use std::{
	marker::PhantomData,
	task::{Context, Poll},
};

use filen_types::{
	api::response::{FilenResponse, ResponseIntoData},
	error::ResponseError,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tower::Service;

use crate::{Error, ErrorKind, auth::http::ResponseType};

use super::{Request, retry::RetryError};

/// Decodes a JSON body. The envelope is decoded by one function shared by every response type,
/// and only `data` is decoded per type, so a decode error's line and column count from the start
/// of `data`.
fn decode_json<Res: DeserializeOwned>(
	bytes: &[u8],
) -> Result<FilenResponse<'_, Res>, RetryError<Error>> {
	decode_json_envelope(bytes)?.try_map_data(|data| {
		serde_json::from_str(data.get()).map_err(|e| json_decode_error(e, bytes.len()))
	})
}

/// The JSON envelope of `bytes`, its `data` left undecoded.
fn decode_json_envelope(bytes: &[u8]) -> Result<FilenResponse<'_, &RawValue>, RetryError<Error>> {
	serde_json::from_slice(bytes).map_err(|e| json_decode_error(e, bytes.len()))
}

/// The error for a JSON body of `len` bytes that did not decode, retryable when it ran out of
/// bytes. `data` is only decoded once the whole envelope has, so only the envelope can run out.
fn json_decode_error(e: serde_json::Error, len: usize) -> RetryError<Error> {
	let truncated = e.classify() == serde_json::error::Category::Eof;
	RetryError::from_retryable(
		truncated,
		Error::custom_with_source(
			ErrorKind::Response,
			e,
			Some(format!("Failed to deserialize json response ({len} bytes)")),
		),
	)
}

/// The gateway msgpack-encodes the same string-keyed structures as its JSON responses (uuids as
/// hyphenated strings, not 16-byte arrays), so format-sensitive types like [`uuid::Uuid`] must be
/// decoded in human-readable mode.
fn from_msgpack_slice<'de, T>(bytes: &'de [u8]) -> Result<T, rmp_serde::decode::Error>
where
	T: Deserialize<'de>,
{
	let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes).with_human_readable();
	T::deserialize(&mut deserializer)
}

/// A decode failure that ran out of bytes means the transport handed us a truncated body (the
/// gateway never streams partial documents), so redriving the request is the fix. A
/// malformed-but-complete body fails on other variants and stays non-retryable.
fn msgpack_error_is_truncation(e: &rmp_serde::decode::Error) -> bool {
	use rmp_serde::decode::Error as DecodeError;
	match e {
		DecodeError::InvalidMarkerRead(io) | DecodeError::InvalidDataRead(io) => {
			io.kind() == std::io::ErrorKind::UnexpectedEof
		}
		_ => false,
	}
}

pub(crate) struct DeserializeLayer<Res> {
	_phantom: std::marker::PhantomData<Res>,
}

impl<Res> Clone for DeserializeLayer<Res> {
	fn clone(&self) -> Self {
		Self {
			_phantom: self._phantom,
		}
	}
}

impl<T> DeserializeLayer<T> {
	pub(crate) fn new() -> Self {
		Self {
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, Res> tower::Layer<S> for DeserializeLayer<Res> {
	type Service = DeserializeService<S, Res>;

	fn layer(&self, inner: S) -> Self::Service {
		DeserializeService {
			inner,
			_phantom: self._phantom,
		}
	}
}

pub(crate) struct DeserializeService<S, Res> {
	inner: S,
	_phantom: std::marker::PhantomData<Res>,
}

impl<S, Res> Clone for DeserializeService<S, Res>
where
	S: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			_phantom: self._phantom,
		}
	}
}

impl<S, Res, Body, Url> Service<Request<Body, Url>> for DeserializeService<S, Res>
where
	S: Service<Request<Body, Url>, Error = super::retry::RetryError<Error>>,
	S::Response: AsRef<[u8]>,
	Res: DeserializeOwned,
{
	type Response = Res;
	type Error = S::Error;
	type Future = DeserializeFuture<S::Future, Res>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Body, Url>) -> Self::Future {
		let response_type = req.response_type;
		DeserializeFuture::new(self.inner.call(req), response_type)
	}
}

#[pin_project::pin_project]
pub(crate) struct DeserializeFuture<F, Res> {
	response_type: ResponseType,
	#[pin]
	fut: Option<F>,
	_phantom: PhantomData<Res>,
}

impl<F, Res> DeserializeFuture<F, Res> {
	fn new(fut: F, response_type: ResponseType) -> Self {
		Self {
			response_type,
			fut: Some(fut),
			_phantom: PhantomData,
		}
	}
}

impl<F, Res, InRes> Future for DeserializeFuture<F, Res>
where
	F: Future<Output = Result<InRes, RetryError<Error>>>,
	InRes: AsRef<[u8]>,
	Res: DeserializeOwned,
{
	type Output = Result<Res, RetryError<Error>>;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		if let Some(fut) = this.fut.as_pin_mut() {
			match fut.poll(cx) {
				Poll::Ready(Ok(res)) => {
					let response: FilenResponse<Res> = if *this.response_type == ResponseType::Large
					{
						match from_msgpack_slice(res.as_ref()) {
							Ok(resp) => resp,
							Err(e) => {
								let truncated = msgpack_error_is_truncation(&e);
								return Poll::Ready(Err(RetryError::from_retryable(
									truncated,
									Error::custom_with_source(
										ErrorKind::Response,
										e,
										Some(format!(
											"Failed to deserialize msgpack response ({} bytes)",
											res.as_ref().len()
										)),
									),
								)));
							}
						}
					} else {
						match decode_json(res.as_ref()) {
							Ok(resp) => resp,
							Err(e) => return Poll::Ready(Err(e)),
						}
					};

					match ResponseIntoData::into_data(response) {
						Ok(data) => Poll::Ready(Ok(data)),
						Err(ResponseError::ApiError { message, code })
							if code.as_deref() == Some("internal_error") =>
						{
							Poll::Ready(Err(RetryError::Retry(
								ResponseError::ApiError { message, code }.into(),
							)))
						}
						Err(e) => Poll::Ready(Err(RetryError::NoRetry(e.into()))),
					}
				}
				Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
				Poll::Pending => Poll::Pending,
			}
		} else {
			panic!("future already completed")
		}
	}
}

#[cfg(test)]
mod tests {
	use filen_types::{api::response::FilenResponse, fs::Uuid};
	use serde::Deserialize;

	#[derive(Deserialize, Debug)]
	struct UuidHolder {
		uuid: Uuid,
	}

	/// The live gateway sends uuids as hyphenated strings inside msgpack responses; decoding them
	/// as [`Uuid`] requires the human-readable deserializer (the default binary mode expects
	/// 16-byte arrays and rejects every real response).
	#[test]
	fn msgpack_response_decodes_string_uuids() {
		let uuid = Uuid::new_v4();
		let bytes = rmp_serde::to_vec(&serde_json::json!({
			"status": true,
			"message": "ok",
			"code": "ok",
			"data": { "uuid": uuid.to_string() },
		}))
		.unwrap();

		let response: FilenResponse<UuidHolder> = super::from_msgpack_slice(&bytes).unwrap();
		assert_eq!(response.into_data().unwrap().uuid, uuid);

		rmp_serde::from_slice::<FilenResponse<UuidHolder>>(&bytes)
			.expect_err("binary-mode decode should reject string uuids; if this now passes, the human-readable workaround may be removable");
	}

	/// A body cut short mid-document must classify as truncation (retryable), while a
	/// complete-but-wrong document must not — a live truncated v3/dir/content response was
	/// surfaced as non-retryable on the 2026-08-16 nightly.
	#[test]
	fn msgpack_truncation_is_distinguished_from_malformed() {
		let uuid = Uuid::new_v4();
		let bytes = rmp_serde::to_vec(&serde_json::json!({
			"status": true,
			"message": "ok",
			"code": "ok",
			"data": { "uuid": uuid.to_string() },
		}))
		.unwrap();

		let err = super::from_msgpack_slice::<FilenResponse<UuidHolder>>(&bytes[..bytes.len() / 2])
			.expect_err("a half body must not decode");
		assert!(
			super::msgpack_error_is_truncation(&err),
			"a mid-document EOF must classify as truncation, got: {err:?}"
		);

		let err = super::from_msgpack_slice::<FilenResponse<UuidHolder>>(
			&rmp_serde::to_vec(&serde_json::json!({ "status": "not-a-bool" })).unwrap(),
		)
		.expect_err("a mistyped document must not decode");
		assert!(
			!super::msgpack_error_is_truncation(&err),
			"a complete but wrong document must not classify as truncation, got: {err:?}"
		);
	}

	/// Layer-level polarity pin: the WIRING must classify a truncated document Retry and a
	/// malformed-but-complete one NoRetry, in both formats. The helper test above cannot catch
	/// someone reverting the call sites back to unconditional NoRetry (the pre-fix code, which
	/// reproduces the 2026-08-16 nightly failure with an otherwise green suite).
	#[test]
	fn deserialize_layer_classifies_truncation_retryable() {
		use crate::auth::http::{ResponseType, retry::RetryError};

		let run = |bytes: Vec<u8>, response_type| {
			futures::executor::block_on(super::DeserializeFuture::<_, UuidHolder>::new(
				std::future::ready(Ok::<_, RetryError<crate::Error>>(bytes)),
				response_type,
			))
		};

		let uuid = Uuid::new_v4();
		let document = serde_json::json!({
			"status": true,
			"message": "ok",
			"code": "ok",
			"data": { "uuid": uuid.to_string() },
		});

		let msgpack = rmp_serde::to_vec(&document).unwrap();
		match run(msgpack[..msgpack.len() / 2].to_vec(), ResponseType::Large) {
			Err(RetryError::Retry(_)) => {}
			other => panic!("a truncated msgpack response must be Retry, got {other:?}"),
		}
		match run(
			rmp_serde::to_vec(&serde_json::json!({ "status": "not-a-bool" })).unwrap(),
			ResponseType::Large,
		) {
			Err(RetryError::NoRetry(_)) => {}
			other => panic!("a mistyped msgpack response must be NoRetry, got {other:?}"),
		}

		let json = serde_json::to_vec(&document).unwrap();
		match run(json[..json.len() / 2].to_vec(), ResponseType::Standard) {
			Err(RetryError::Retry(_)) => {}
			other => panic!("a truncated json response must be Retry, got {other:?}"),
		}
		match run(
			b"{\"status\": \"not-a-bool\"}".to_vec(),
			ResponseType::Standard,
		) {
			Err(RetryError::NoRetry(_)) => {}
			other => panic!("a mistyped json response must be NoRetry, got {other:?}"),
		}
	}
}
