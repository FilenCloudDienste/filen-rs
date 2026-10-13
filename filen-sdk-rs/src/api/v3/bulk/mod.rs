//! `POST /v3/bulk/*`: one request runs a single route for many items. [`send`] posts one chunk and
//! [`run_each`] turns its answers into the results the single calls would have given.

use std::marker::PhantomData;

use filen_types::{
	api::{response::FilenResponse, v3::bulk::Response},
	error::ResponseError,
};
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::error::{Error, ErrorKind};

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests;

/// What a bulk request carries besides its items, which decides what a whole-request
/// `invalid_params` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Body {
	/// `{"items": [...]}` alone: `invalid_params` rejects the request for one bad item, so the
	/// single calls still do the work and isolate it. A malformed API key answers
	/// `invalid_params` too; every single call then fails with it as well.
	ItemsOnly,
	/// Top-level fields beside `items` (`share`'s `email`, `dir/link/add`'s link): `invalid_params`
	/// can be about them, and then every single call would fail the same way.
	WithTopLevelFields,
}

/// The outcome of a chunk the server took or turned away. A transport error, a timeout or a
/// whole-request error is not one: it is returned as the chunk's error, and some unknown prefix
/// of the items may have run.
pub(crate) enum Sent<'a, I, T> {
	/// The `bulk_processed` answer: each item with its single route's envelope, in input order.
	Answers(Vec<(&'a I, Answer<T>)>),
	/// No item ran: the server does not have the endpoint (`invalid_endpoint`, which is not
	/// cached, since the next request may reach a server that has it), or turned the items of a
	/// [`Body::ItemsOnly`] request away (`invalid_params`). Run the single calls, in input order.
	Unsupported(&'a [I]),
}

/// One item's envelope, its `data` not yet decoded as `T`: [`run_each`] decodes each item on its
/// own, so data that does not decode fails that item alone and the chunk's other items keep their
/// results.
pub(crate) struct Answer<T> {
	envelope: FilenResponse<'static, Box<RawValue>>,
	data: PhantomData<fn() -> T>,
}

/// Why a chunk failed before or after it was sent.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum BulkError {
	/// The caller handed over more items than the endpoint takes in one request; nothing was
	/// sent. Splitting the work into chunks is the caller's job.
	#[error("{items} items exceed the endpoint's limit of {cap} per request")]
	TooManyItems { items: usize, cap: usize },
	/// The server answered a different number of results than items were sent.
	#[error("the server answered {results} results for {items} items")]
	ResultCount { items: usize, results: usize },
}

impl From<BulkError> for Error {
	fn from(e: BulkError) -> Self {
		let kind = match e {
			BulkError::TooManyItems { .. } => ErrorKind::Internal,
			BulkError::ResultCount { .. } => ErrorKind::Response,
		};
		Error::custom_with_source(kind, e, None::<&str>)
	}
}

/// Sends one chunk of `items` with `post`, at most `cap` of them. `post` is awaited at most once
/// and not at all when `items` is empty or over `cap`. It is not retried: the stack already
/// retries a whole-request `internal_error`, under which nothing ran. An `Err` may follow a chunk
/// whose items all ran (see [`Sent`]), so the caller handles it like a lost response.
pub(crate) async fn send<'a, I, T>(
	items: &'a [I],
	cap: usize,
	body: Body,
	post: impl Future<Output = Result<Response<'static, Box<RawValue>>, Error>>,
) -> Result<Sent<'a, I, T>, Error> {
	if items.len() > cap {
		return Err(BulkError::TooManyItems {
			items: items.len(),
			cap,
		}
		.into());
	}
	if items.is_empty() {
		// the server would turn an empty list away as `invalid_params`
		return Ok(Sent::Answers(Vec::new()));
	}
	match post.await {
		Ok(Response { results }) if results.len() == items.len() => Ok(Sent::Answers(
			items
				.iter()
				.zip(results.into_iter().map(|envelope| Answer {
					envelope,
					data: PhantomData,
				}))
				.collect(),
		)),
		Ok(Response { results }) => Err(BulkError::ResultCount {
			items: items.len(),
			results: results.len(),
		}
		.into()),
		Err(e) if nothing_ran(&e, body) => Ok(Sent::Unsupported(items)),
		Err(e) => Err(e),
	}
}

/// Whether `error` is a whole-request answer under which no item ran and the single calls can do
/// the work.
fn nothing_ran(error: &Error, body: Body) -> bool {
	let Some(ResponseError::ApiError { code, .. }) = error.downcast_ref::<ResponseError>() else {
		return false;
	};
	match code.as_deref() {
		Some("invalid_endpoint") => true,
		Some("invalid_params") => body == Body::ItemsOnly,
		_ => false,
	}
}

/// The `data` an item's answer carries, decoded by the rule the single routes' deserialize layer
/// applies.
pub(crate) trait ItemData: Sized {
	/// The item's `Ok`, or its error with the kind and server code its single call would give.
	fn from_answer(envelope: FilenResponse<'static, Box<RawValue>>) -> Result<Self, Error>;
}

impl<T: DeserializeOwned> ItemData for T {
	default fn from_answer(envelope: FilenResponse<'static, Box<RawValue>>) -> Result<Self, Error> {
		let data = envelope.into_data()?;
		serde_json::from_str(data.get()).map_err(|e| {
			// the kind a single call gets for a response it cannot decode
			Error::custom_with_source(
				ErrorKind::Response,
				e,
				Some("failed to deserialize a bulk item's data"),
			)
		})
	}
}

/// A route without data succeeds on `status: true` whatever `data` holds, as `ResponseIntoData`
/// does for `()`.
impl ItemData for () {
	fn from_answer(envelope: FilenResponse<'static, Box<RawValue>>) -> Result<Self, Error> {
		Ok(envelope.ignore_data()?)
	}
}

/// Each item's result, mapped by the rule the single call's answer goes through, so it has the
/// single call's `Ok`, or its error's kind and server code. An item answered `internal_error` runs
/// once as its `single` call, which the stack retries like any other; under
/// [`Sent::Unsupported`] every item runs as its `single` call. Single calls run one at a time, in
/// input order, after the whole chunk: only for items that do not depend on each other.
pub(crate) async fn run_each<'a, I, T, F, Fut>(
	sent: Sent<'a, I, T>,
	mut single: F,
) -> Vec<Result<T, Error>>
where
	T: ItemData,
	F: FnMut(&'a I) -> Fut,
	Fut: Future<Output = Result<T, Error>>,
{
	match sent {
		Sent::Answers(answers) => {
			let mut results = Vec::with_capacity(answers.len());
			for (item, answer) in answers {
				results.push(match T::from_answer(answer.envelope) {
					Err(e)
						if e.downcast_ref::<ResponseError>().is_some_and(
							|ResponseError::ApiError { code, .. }| {
								code.as_deref() == Some("internal_error")
							},
						) =>
					{
						single(item).await
					}
					result => result,
				});
			}
			results
		}
		Sent::Unsupported(items) => {
			let mut results = Vec::with_capacity(items.len());
			for item in items {
				results.push(single(item).await);
			}
			results
		}
	}
}
