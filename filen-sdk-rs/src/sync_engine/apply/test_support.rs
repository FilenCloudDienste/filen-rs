use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use tokio::{io::AsyncWriteExt, net::TcpListener};
use uuid::Uuid;

use super::{
	super::{
		baseline::BaselineEntry,
		plan::{RemoteNode, SyncAction},
		rows::Baseline,
		scan::LocalNode,
		side::Side,
		stage::{Screen, Stage, StagedPlan},
	},
	PassStart, PlannedEntry, StartInputs,
};
use crate::auth::http::{ServerAnswers, test_support::read_head};

/// A pass that held nothing at any path.
struct Unread;

impl StartInputs for Unread {
	fn local(&mut self, _: &str) -> Option<Box<LocalNode>> {
		None
	}

	fn remote(&mut self, _: &str) -> Option<Box<RemoteNode>> {
		None
	}

	fn base(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		None
	}

	fn base_from(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		None
	}

	fn parent_uuid(&mut self, _: &str) -> Option<Uuid> {
		None
	}
}

impl PassStart {
	/// What `action`'s kind reads, with nothing read at any of it.
	pub(in crate::sync_engine) fn bare(action: &SyncAction) -> Self {
		Self::of(action, &mut Unread)
	}
}

impl PlannedEntry {
	/// `actions`, a plan as it stands, staged as [`stage`](Self::stage) stages a screened one.
	pub(in crate::sync_engine) fn stage_actions(
		actions: &[SyncAction],
		baseline: Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
		stage: Stage,
	) -> rusqlite::Result<StagedPlan> {
		Self::stage(Screen::new(stage, &[], actions)?, baseline, local, remote)
	}

	/// `action`, with nothing read for it (see [`PassStart::bare`]).
	pub(in crate::sync_engine) fn bare(action: SyncAction) -> Self {
		Self {
			start: PassStart::bare(&action),
			action,
		}
	}
}

/// The error of a request a local server answered with `status`.
pub(in crate::sync_engine) async fn status_error(status: u16) -> reqwest::Error {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		read_head(&mut socket).await;
		socket
			.write_all(format!("HTTP/1.1 {status} Status\r\nContent-Length: 0\r\n\r\n").as_bytes())
			.await
			.unwrap();
		socket
	});
	let response = reqwest::Client::builder()
		.no_proxy()
		.build()
		.unwrap()
		.get(format!("http://{addr}/"))
		.send()
		.await
		.unwrap();
	server.await.unwrap();
	response.error_for_status().unwrap_err()
}

/// The error of a connect to a local port nothing listens on.
pub(in crate::sync_engine) async fn refused_error() -> reqwest::Error {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	drop(listener);
	reqwest::Client::builder()
		.no_proxy()
		.build()
		.unwrap()
		.get(format!("http://{addr}/"))
		.send()
		.await
		.unwrap_err()
}

pub(in crate::sync_engine) fn session_refused() -> crate::Error {
	filen_types::error::ResponseError::ApiError {
		message: None,
		code: Some("api_key_not_found".to_string()),
	}
	.into()
}

/// Be the HTTP proxy of a client whose servers turn away every request of this pass: each
/// `CONNECT` is refused with a 502. With `answers`, an answer from the server it was for is noted
/// there first, as one to another request of the client while this one ran would be. Returns the
/// proxy's task and how many requests reached it.
pub(in crate::sync_engine) fn refuse_every_request(
	listener: TcpListener,
	answers: Option<Arc<ServerAnswers>>,
) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
	let requests = Arc::new(AtomicUsize::new(0));
	let counted = Arc::clone(&requests);
	let proxy = tokio::spawn(async move {
		while let Ok((mut socket, _)) = listener.accept().await {
			let answers = answers.clone();
			let counted = Arc::clone(&counted);
			tokio::spawn(async move {
				let head = read_head(&mut socket).await;
				if let Some(answers) = answers {
					let host = head
						.strip_prefix("CONNECT ")
						.and_then(|target| target.split(':').next())
						.expect("an https request through a proxy is a CONNECT");
					answers.note_answer_from(host);
				}
				counted.fetch_add(1, Ordering::SeqCst);
				socket
					.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
					.await
					.ok();
			});
		}
	});
	(proxy, requests)
}
