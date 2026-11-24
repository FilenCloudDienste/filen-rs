use std::{
	sync::{Mutex, PoisonError},
	task::{Context, Poll},
};

use filen_types::{
	api::response::{FilenResponse, ResponseIntoData},
	error::ResponseError,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tower::Service;

use crate::{
	Error, ErrorKind,
	auth::http::ResponseType,
	util::{MaybeSend, MaybeSendSync},
};

use super::{Request, retry::RetryError};

/// How a [`DeserializeLayer`] decodes a response body, fixed per stack at compile time. It also
/// names the [`ResponseType`] the layer asks the server for, so the request and the decoder cannot
/// disagree: a stack is generic over the format alone, and decodes into the response's own type
/// through a [`ResponseSink`] of that same format.
pub(crate) trait WireFormat {
	/// What [`DeserializeService::call`] writes into the request, which picks its headers.
	const RESPONSE_TYPE: ResponseType;

	/// Decodes `bytes`; a failure is retryable when the body was cut short.
	fn decode<Res: DeserializeOwned>(
		bytes: &[u8],
	) -> Result<FilenResponse<'_, Res>, RetryError<Error>>;
}

/// The gateway's default JSON body.
pub(crate) struct Json;

/// The msgpack body the gateway sends when the request carries the `msgpack` header.
pub(crate) struct Msgpack;

impl WireFormat for Json {
	const RESPONSE_TYPE: ResponseType = ResponseType::Standard;

	/// The envelope is decoded by one function shared by every response type, and only `data` is
	/// decoded per type, so a decode error's line and column count from the start of `data`.
	fn decode<Res: DeserializeOwned>(
		bytes: &[u8],
	) -> Result<FilenResponse<'_, Res>, RetryError<Error>> {
		decode_json_envelope(bytes)?.try_map_data(|data| {
			serde_json::from_str(data.get()).map_err(|e| json_decode_error(e, bytes.len()))
		})
	}
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

impl WireFormat for Msgpack {
	const RESPONSE_TYPE: ResponseType = ResponseType::Large;

	fn decode<Res: DeserializeOwned>(
		bytes: &[u8],
	) -> Result<FilenResponse<'_, Res>, RetryError<Error>> {
		from_msgpack_slice(bytes).map_err(|e| {
			let truncated = msgpack_error_is_truncation(&e);
			RetryError::from_retryable(
				truncated,
				Error::custom_with_source(
					ErrorKind::Response,
					e,
					Some(format!(
						"Failed to deserialize msgpack response ({} bytes)",
						bytes.len()
					)),
				),
			)
		})
	}
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

/// Decodes `bytes` into the answer's `data`: a body cut short and the server's `internal_error`
/// answer are worth sending the request again for, a document that does not decode into `Res` and
/// any other error answer are not.
fn decode_response<W: WireFormat, Res: DeserializeOwned>(
	bytes: &[u8],
) -> Result<Res, RetryError<Error>> {
	let response: FilenResponse<Res> = W::decode(bytes)?;
	match ResponseIntoData::into_data(response) {
		Ok(data) => Ok(data),
		Err(ResponseError::ApiError { message, code })
			if code.as_deref() == Some("internal_error") =>
		{
			Err(RetryError::Retry(
				ResponseError::ApiError { message, code }.into(),
			))
		}
		Err(e) => Err(RetryError::NoRetry(e.into())),
	}
}

/// Where a stack of format `W` decodes each attempt's body to, inside the retry loop. The stack
/// sees it as a trait object, so it is compiled once per format whatever the response type, and
/// only [`ResponseSlot`]'s small impl is compiled per response type.
pub(crate) trait ResponseSink<W>: MaybeSendSync {
	/// Decodes `bytes` with format `W` (see [`decode_response`]) and keeps the answer.
	fn accept(&self, bytes: &[u8]) -> Result<(), RetryError<Error>>;
}

/// The response an entry point waits for, decoded by its stack's [`DeserializeLayer`]. The entry
/// point creates it for its own response type, lends it to the stack it sends the request
/// through, and takes the answer out once the stack has answered, so the type a body is decoded
/// into is the one the entry point returns.
pub(crate) struct ResponseSlot<Res> {
	/// A lock because the stack's future, which borrows the slot, must stay `Send`. Only one
	/// attempt decodes at a time, so it is never contended.
	response: Mutex<Option<Res>>,
}

impl<Res> ResponseSlot<Res> {
	pub(crate) fn new() -> Self {
		Self {
			response: Mutex::new(None),
		}
	}

	/// The answer the stack decoded. `accepted` is what a stack answers with, and only a
	/// [`DeserializeFuture`] that just filled a slot makes one.
	pub(crate) fn into_response(self, accepted: Accepted) -> Result<Res, Error> {
		let Accepted(()) = accepted;
		self.response
			.into_inner()
			// the lock is only held to store a decoded value, which cannot panic
			.unwrap_or_else(PoisonError::into_inner)
			.ok_or_else(|| {
				Error::custom(
					ErrorKind::Internal,
					"a request's stack answered without decoding a response (should be impossible)",
				)
			})
	}
}

impl<W, Res> ResponseSink<W> for ResponseSlot<Res>
where
	W: WireFormat,
	Res: DeserializeOwned + MaybeSend,
{
	fn accept(&self, bytes: &[u8]) -> Result<(), RetryError<Error>> {
		let response = decode_response::<W, Res>(bytes)?;
		*self.response.lock().unwrap_or_else(PoisonError::into_inner) = Some(response);
		Ok(())
	}
}

/// What a stack answers with once its [`DeserializeLayer`] has decoded a body into the
/// [`ResponseSlot`] it was lent; [`ResponseSlot::into_response`] takes it.
pub(crate) struct Accepted(());

/// Asks the server for format `W` and decodes every attempt's body into its [`ResponseSink`],
/// below the retry layer, so a body cut short or an `internal_error` answer is sent again.
pub(crate) struct DeserializeLayer<'a, W> {
	sink: &'a dyn ResponseSink<W>,
}

impl<W> Clone for DeserializeLayer<'_, W> {
	fn clone(&self) -> Self {
		Self { sink: self.sink }
	}
}

impl<'a, W> DeserializeLayer<'a, W> {
	pub(crate) fn new(sink: &'a dyn ResponseSink<W>) -> Self {
		Self { sink }
	}
}

impl<'a, S, W> tower::Layer<S> for DeserializeLayer<'a, W> {
	type Service = DeserializeService<'a, S, W>;

	fn layer(&self, inner: S) -> Self::Service {
		DeserializeService {
			inner,
			sink: self.sink,
		}
	}
}

/// See [`DeserializeLayer`].
pub(crate) struct DeserializeService<'a, S, W> {
	inner: S,
	sink: &'a dyn ResponseSink<W>,
}

impl<S, W> Clone for DeserializeService<'_, S, W>
where
	S: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			sink: self.sink,
		}
	}
}

impl<'a, S, W, Body, Url> Service<Request<Body, Url>> for DeserializeService<'a, S, W>
where
	S: Service<Request<Body, Url>, Response = Vec<u8>, Error = RetryError<Error>>,
	W: WireFormat,
{
	type Response = Accepted;
	type Error = S::Error;
	type Future = DeserializeFuture<'a, S::Future, W>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Body, Url>) -> Self::Future {
		let req = Request {
			response_type: W::RESPONSE_TYPE,
			..req
		};
		DeserializeFuture {
			fut: self.inner.call(req),
			sink: self.sink,
		}
	}
}

/// See [`DeserializeLayer`].
#[pin_project::pin_project]
pub(crate) struct DeserializeFuture<'a, F, W> {
	#[pin]
	fut: F,
	sink: &'a dyn ResponseSink<W>,
}

impl<F, W> Future for DeserializeFuture<'_, F, W>
where
	F: Future<Output = Result<Vec<u8>, RetryError<Error>>>,
{
	type Output = Result<Accepted, RetryError<Error>>;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		let body = std::task::ready!(this.fut.poll(cx))?;
		this.sink.accept(&body)?;
		Poll::Ready(Ok(Accepted(())))
	}
}

#[cfg(test)]
mod tests {
	use std::cell::Cell;

	use filen_types::{api::response::FilenResponse, fs::Uuid};
	use serde::Deserialize;
	use tower::{Layer, Service};

	use super::{DeserializeFuture, DeserializeLayer, Json, Msgpack, ResponseSlot, WireFormat};
	use crate::auth::http::{Request, RequestMethod, ResponseType, retry::RetryError};

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
		fn run<W: WireFormat>(bytes: Vec<u8>) -> Result<UuidHolder, RetryError<crate::Error>> {
			let slot = ResponseSlot::<UuidHolder>::new();
			let accepted = futures::executor::block_on(DeserializeFuture::<_, W> {
				fut: std::future::ready(Ok::<_, RetryError<crate::Error>>(bytes)),
				sink: &slot,
			})?;
			Ok(slot.into_response(accepted).unwrap())
		}

		let uuid = Uuid::new_v4();
		let document = serde_json::json!({
			"status": true,
			"message": "ok",
			"code": "ok",
			"data": { "uuid": uuid.to_string() },
		});

		let msgpack = rmp_serde::to_vec(&document).unwrap();
		match run::<Msgpack>(msgpack[..msgpack.len() / 2].to_vec()) {
			Err(RetryError::Retry(_)) => {}
			other => panic!("a truncated msgpack response must be Retry, got {other:?}"),
		}
		match run::<Msgpack>(
			rmp_serde::to_vec(&serde_json::json!({ "status": "not-a-bool" })).unwrap(),
		) {
			Err(RetryError::NoRetry(_)) => {}
			other => panic!("a mistyped msgpack response must be NoRetry, got {other:?}"),
		}

		let json = serde_json::to_vec(&document).unwrap();
		match run::<Json>(json[..json.len() / 2].to_vec()) {
			Err(RetryError::Retry(_)) => {}
			other => panic!("a truncated json response must be Retry, got {other:?}"),
		}
		match run::<Json>(b"{\"status\": \"not-a-bool\"}".to_vec()) {
			Err(RetryError::NoRetry(_)) => {}
			other => panic!("a mistyped json response must be NoRetry, got {other:?}"),
		}
	}

	/// The request a stack sends asks for the format its layer decodes, whatever the request
	/// carried before: the `msgpack` header follows from [`WireFormat::RESPONSE_TYPE`].
	#[test]
	fn deserialize_service_asks_for_the_format_it_decodes() {
		fn sent_response_type<W: WireFormat>(carried: ResponseType) -> ResponseType {
			let sent = Cell::new(None);
			let inner = tower::service_fn(|req: Request<(), ()>| {
				sent.set(Some(req.response_type));
				std::future::ready(Ok::<_, RetryError<crate::Error>>(Vec::<u8>::new()))
			});
			let slot = ResponseSlot::<()>::new();
			let mut service = DeserializeLayer::<W>::new(&slot).layer(inner);
			let decoded = futures::executor::block_on(service.call(Request {
				method: RequestMethod::Get,
				response_type: carried,
				url: (),
				client: reqwest::Client::new(),
			}));
			// only the request the inner service saw matters here
			assert!(decoded.is_err(), "an empty body must not decode");
			sent.get().expect("the inner service was called")
		}

		for carried in [ResponseType::Standard, ResponseType::Large] {
			assert_eq!(sent_response_type::<Json>(carried), ResponseType::Standard);
			assert_eq!(sent_response_type::<Msgpack>(carried), ResponseType::Large);
		}
	}
}
