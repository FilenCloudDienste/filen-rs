//! Whether an action that failed on a server's transient answer failed because the remote is down
//! or because of its own path: the policy behind [`SyncReport::halted`](super::SyncReport::halted)
//! for [`HaltReason::RemoteUnavailable`].
//!
//! A failure whose error says the remote cannot serve anything for now (see `pass_halt`) does not
//! say whether it cannot serve ANYTHING: a server answering 5xx, 408 or 429, or not answering at
//! all, looks the same to the request whether the whole service is down or the server will not
//! serve that one path. The two call for opposite things. An outage should stop the pass starting
//! more remote work and count against no path, or every transfer it catches is blocked for an hour
//! after it. A path the server always fails should count against that path, and stop nothing else,
//! or it holds back the pair's remote work on every pass until its streak runs out.
//!
//! So the pass asks whether the server was shown to be answering when the failure came in:
//!
//! - A session the server refuses ([`HaltReason::Unauthenticated`]) always halts the pass. No
//!   request can succeed without one, whoever else the server answers.
//! - A transient failure ([`HaltReason::RemoteUnavailable`]: a 5xx, 408 or 429, a failed connect, a
//!   timeout, a lost connection) halts the pass only while the server its request went to has not
//!   been shown answering. Once it has, the failure is its path's own: counted in full against the
//!   path's streak like any other failure, and halting nothing.
//!
//! A server is shown answering by an actual answer from it ([`ServerAnswers`]): a response the
//! client received, noted where the HTTP response arrives, with a status that is not itself
//! transient. Nothing an action finishes without a request (an empty file's download, a local
//! write) shows anything, and neither does an answer from another server: the gateway answering
//! says nothing about the server downloads come from.
//!
//! The answer has to have come in WHILE the failing action ran, from its start to the moment its
//! failure is classified, rather than at any time in the pass. An answer from before the action
//! began says the server was up then, not that it still is: a pass that lost its network halfway
//! would otherwise count every action after the loss against its path, and wait out every one of
//! their retries and timeouts instead of halting. As it is, an outage that begins mid-pass costs a
//! full count only to the actions already in flight that had an answer while they ran (at most the
//! client's concurrency), and the first action after them halts the pass. A path the server always
//! fails is shown up by the other transfers that run beside it, whose answers come in while its
//! own retries run. A failure of a gateway request no other answer came in for is settled by
//! asking the gateway for its health ([`health`]) right then: that is the only request a remote
//! directory create, move or trash sends, and they run one at a time, so nothing else would show
//! the gateway up. Failures that come in together share one such probe: once one goes unanswered,
//! the gateway counts as not answering for the rest of the pass, until it answers anything.
//!
//! The failure of a body cut short after its head names its server as any other does: the body
//! layers give the error the URL of the response it read (see `auth::http::download_body`). The
//! failing request's own head is an answer from that server that came in while the action ran,
//! so such a failure is its path's own: the server answered, and then the body did not arrive.
//!
//! A bulk request that failed as a whole, every item it carried answered with one shared error,
//! is one failure, not one per item (see `classified`): classified once, and when its server was
//! shown answering, counted spaced out against each item rather than in full, since nothing says
//! which item, if any, the server would not serve.
//!
//! A failure is classified once, as it comes in, and stays as classified. A success or an answer
//! after it does not make a failure the pass took for an outage its path's, so a transfer landing
//! after the pass halted charges none of the failures before it.
//!
//! What this cannot tell apart, or tells apart only roughly:
//!
//! - A transfer whose server answered nothing else while it ran, when every transfer to that
//!   server in flight beside it failed too (a run of failing files at least the client's
//!   concurrency long, or a concurrency of one), is taken for that server's outage. The downloads
//!   and uploads servers have no health a probe could ask without the gateway's answer standing
//!   in for theirs. Such a failure counts only spaced out (see the engine's
//!   `TRANSIENT_FAILURE_SPACING`), which still blocks a path that always fails that way.
//! - A halt is the pass's, not the server's: one server taken for down holds back the remote
//!   actions not started yet on every server, the gateway's directory creates, moves and trashes
//!   and the transfers to the other server included, until the next pass. The transfers already
//!   in flight finish. So the transfer above, failing alone at the end of a pass's transfers, also
//!   holds back the remote trashes that run after them, on every pass until its streak blocks it.
//!   Holding back only what goes to the server taken for down would need the halt kept per server.
//! - A 429 (or a 408) is a server turning the CLIENT away, not the path, yet once the server
//!   answers a neighbour it counts against the path in full like a 5xx. It is left so on purpose:
//!   the gateway signals rate limiting as a 200 with its own `rate_limited` code, which never gets
//!   here, a 429 has to outlast every retry of its request to be charged at all, and a path is
//!   blocked only after doing so on three passes running with no success between.

use std::time::Duration;

use tokio::{sync::Mutex, time::timeout};

use super::{pass_halt, transient_server};
use crate::{
	api::v3::health,
	auth::{Client, http::AnswerMark},
	consts::random_gateway_url,
	sync_engine::outcome::HaltReason,
};

/// How long a probe of the gateway's health may take before the gateway counts as not answering.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// What one pass has seen of the servers it talks to answering (see the module docs).
pub(super) struct Reach<'a> {
	client: &'a Client,
	/// Where the client's answers stood as the latest probe of the gateway that got no answer was
	/// sent: until the gateway answers anything after it, a failure on it needs no probe of its
	/// own.
	unanswered_probe: Mutex<Option<AnswerMark>>,
}

impl<'a> Reach<'a> {
	pub(super) fn new(client: &'a Client) -> Self {
		Self {
			client,
			unanswered_probe: Mutex::new(None),
		}
	}

	/// Where the client's answers stand now: what an action takes as it starts, for [`halt`].
	///
	/// [`halt`]: Self::halt
	pub(super) fn mark(&self) -> AnswerMark {
		self.client.unauthed().state().answers().mark()
	}

	/// What `error`, the failure of an action that started at `since`, does to the pass: the
	/// condition it halts the pass on, or `None` for a failure that is its path's own. A failure on
	/// a transient answer halts it only when its server was not shown answering since `since` (see
	/// the module docs).
	pub(super) async fn halt(&self, error: &crate::Error, since: AnswerMark) -> Option<HaltReason> {
		match pass_halt(error) {
			Some(HaltReason::RemoteUnavailable)
				if self.answering(transient_server(error), since).await =>
			{
				None
			}
			halt => halt,
		}
	}

	/// Whether `server`, which a request failed on transiently, answered a request of the client
	/// after `since`, asking the gateway for its health if it is the gateway and nothing else
	/// answered. A failure that names no server shows none answering.
	async fn answering(&self, server: Option<&str>, since: AnswerMark) -> bool {
		let Some(server) = server else {
			return false;
		};
		let answers = self.client.unauthed().state().answers();
		if answers.answered_since(server, since) {
			return true;
		}
		if !is_gateway(server) {
			return false;
		}
		let mut unanswered = self.unanswered_probe.lock().await;
		// Another failure's probe may have been answered while this one waited for its turn.
		if answers.answered_since(server, since) {
			return true;
		}
		if unanswered.is_some_and(|sent| !answers.answered_since(server, sent)) {
			return false;
		}
		let sent = answers.mark();
		// Only its answer matters, which the client notes as it arrives; a body the probe could not
		// read, or a refusal, is still the gateway answering.
		if timeout(PROBE_TIMEOUT, health::post(self.client.client()))
			.await
			.is_err()
		{
			tracing::debug!("apply: the gateway's health did not answer within {PROBE_TIMEOUT:?}");
		}
		if answers.answered_since(server, since) {
			return true;
		}
		*unanswered = Some(sent);
		false
	}
}

/// Whether `server` is the gateway, the host of every API request.
fn is_gateway(server: &str) -> bool {
	reqwest::Url::parse(random_gateway_url())
		.is_ok_and(|gateway| gateway.host_str() == Some(server))
}

#[cfg(test)]
mod tests;
