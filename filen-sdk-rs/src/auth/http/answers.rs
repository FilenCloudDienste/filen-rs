//! Which servers this client has heard back from, and in what order: the record that tells a
//! server that is down from one that answers but will not serve one request (the sync engine's
//! `apply::reach`).

use std::{
	collections::HashMap,
	sync::{Mutex, PoisonError},
};

use reqwest::{StatusCode, Url};

use super::is_transient_status;

/// The answers this client's requests got, by the server that sent each one, in the order they
/// came in.
///
/// An answer is a response the server sent with a status other than one that says it cannot serve
/// anything for now ([`is_transient_status`]: a 5xx, 408 or 429): a success, and also a refusal of
/// the request itself such as a 404, which is the server answering. A request that got no response
/// at all, or only a transient status, is no answer, and neither is anything a caller finished
/// without a request, such as an empty file's download.
///
/// Shared by every client cloned from the same one, so the cache worker's requests and every
/// pair's count alike. Kept only in builds with the sync engine, its one reader.
#[derive(Debug, Default)]
pub(crate) struct ServerAnswers {
	heard: Mutex<Heard>,
}

#[derive(Debug, Default)]
struct Heard {
	/// How many answers came in so far: the mark of the latest.
	count: u64,
	/// The mark of the latest answer from each server, by host.
	latest: HashMap<String, u64>,
}

/// A point in the order the answers came in (see [`ServerAnswers::mark`]). Read only by the sync
/// engine.
#[cfg(any(feature = "sync-engine", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnswerMark(u64);

impl ServerAnswers {
	/// Record the response `status` the server of `url` sent, if it is an answer. Nothing but the
	/// sync engine reads the answers, so a build without it keeps none and takes no lock here.
	pub(crate) fn note(&self, url: &Url, status: StatusCode) {
		if !cfg!(any(feature = "sync-engine", test)) || is_transient_status(status) {
			return;
		}
		if let Some(host) = url.host_str() {
			self.note_host(host);
		}
	}

	/// Record an answer from the server `host`.
	fn note_host(&self, host: &str) {
		let mut heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
		// A u64 of answers does not run out; saturating keeps the order if it ever did.
		heard.count = heard.count.saturating_add(1);
		let count = heard.count;
		match heard.latest.get_mut(host) {
			Some(latest) => *latest = count,
			None => {
				heard.latest.insert(host.to_owned(), count);
			}
		}
	}

	/// Where the order of answers stands now: an answer noted after this call is after the mark.
	#[cfg(any(feature = "sync-engine", test))]
	pub(crate) fn mark(&self) -> AnswerMark {
		AnswerMark(
			self.heard
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.count,
		)
	}

	/// Whether the server `host` answered a request after `since`.
	#[cfg(any(feature = "sync-engine", test))]
	pub(crate) fn answered_since(&self, host: &str, since: AnswerMark) -> bool {
		self.heard
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.latest
			.get(host)
			.is_some_and(|latest| *latest > since.0)
	}
}

#[cfg(all(test, feature = "sync-engine"))]
pub(crate) mod test_support {
	use super::ServerAnswers;

	impl ServerAnswers {
		/// Record an answer from the server `host`, as a response from it would.
		pub(crate) fn note_answer_from(&self, host: &str) {
			self.note_host(host);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn url(host: &str) -> Url {
		Url::parse(&format!("https://{host}/v3/thing")).unwrap()
	}

	#[test]
	fn an_answer_counts_for_its_own_server_after_the_mark_only() {
		let answers = ServerAnswers::default();
		answers.note(&url("gateway.filen.io"), StatusCode::OK);
		let mark = answers.mark();
		assert!(!answers.answered_since("gateway.filen.io", mark));

		answers.note(&url("egest.filen.io"), StatusCode::OK);
		assert!(answers.answered_since("egest.filen.io", mark));
		assert!(
			!answers.answered_since("gateway.filen.io", mark),
			"an answer from another server says nothing about this one"
		);
		assert!(!answers.answered_since("ingest.filen.io", mark));
	}

	#[test]
	fn a_refusal_of_the_request_is_an_answer_and_a_transient_status_is_not() {
		let answers = ServerAnswers::default();
		let mark = answers.mark();
		for status in [500, 502, 503, 504, 408, 429] {
			answers.note(
				&url("egest.filen.io"),
				StatusCode::from_u16(status).unwrap(),
			);
		}
		assert!(!answers.answered_since("egest.filen.io", mark));

		answers.note(&url("egest.filen.io"), StatusCode::NOT_FOUND);
		assert!(answers.answered_since("egest.filen.io", mark));
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod sent_tests {
	use tokio::{io::AsyncWriteExt, net::TcpListener};

	use crate::auth::http::{
		ClientConfig,
		test_support::{client, read_head, serve_once},
	};

	/// GET `url` the way a chunk download does, through every layer a request goes through.
	async fn get(
		client: &crate::auth::http::AuthClient,
		url: &str,
	) -> Result<Vec<u8>, crate::Error> {
		client
			.unauthed
			.get_raw_bytes_with_callback(url, "test".into(), None, None::<&fn(u64, Option<u64>)>)
			.await
	}

	#[tokio::test]
	async fn a_response_a_request_gets_is_its_servers_answer() {
		let client = client(ClientConfig::default());
		let answers = client.unauthed.state().answers();
		let mark = answers.mark();
		let (url, server) = serve_once("ok").await;
		get(&client, &url).await.unwrap();
		server.await.unwrap();
		assert!(answers.answered_since("127.0.0.1", mark));
	}

	/// A server that answers every attempt with a 503, and one nothing listens on: neither is an
	/// answer, however many times the retries ask.
	#[tokio::test(start_paused = true)]
	async fn a_transient_status_or_no_response_is_no_answer() {
		let client = client(ClientConfig::default());
		let answers = client.unauthed.state().answers();
		let mark = answers.mark();

		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				read_head(&mut socket).await;
				socket
					.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n")
					.await
					.unwrap();
			}
		});
		get(&client, &format!("http://{addr}/")).await.unwrap_err();
		server.abort();
		assert!(!answers.answered_since("127.0.0.1", mark));

		let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = closed.local_addr().unwrap();
		drop(closed);
		get(&client, &format!("http://{addr}/")).await.unwrap_err();
		assert!(!answers.answered_since("127.0.0.1", mark));
	}
}
