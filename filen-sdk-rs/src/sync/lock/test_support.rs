use std::{
	borrow::Cow,
	net::SocketAddr,
	sync::{Arc, RwLock, atomic::AtomicU64},
};

use filen_types::{auth::APIKey, fs::Uuid};
use tokio::runtime::{Builder, Handle, Runtime};

use super::{Confirmed, ResourceLock, SharedLease};
use crate::auth::{
	http::{AuthClient, ClientConfig, test_support::unauthed_via_proxy},
	unauth::{UnauthClient, test_support::offline_unauthed},
};

/// `unauthed`, with an API key nothing checks.
fn client_from(unauthed: UnauthClient) -> Arc<AuthClient> {
	Arc::new(AuthClient::from_unauthed(
		unauthed,
		Arc::new(RwLock::new(APIKey(Cow::Borrowed("")))),
	))
}

/// A client whose every request fails to connect without leaving the machine: a release or
/// refresh a test sets off must never reach the gateway.
pub(crate) fn offline_client() -> Arc<AuthClient> {
	client_from(offline_unauthed())
}

/// A client whose every request goes to the HTTP proxy at `proxy`, where a test sees it arrive.
pub(crate) fn proxied_client(proxy: SocketAddr) -> Arc<AuthClient> {
	client_from(unauthed_via_proxy(
		ClientConfig::default(),
		&format!("http://{proxy}"),
	))
}

/// A valid lock whose release, when dropped, goes to `handle` (or the ambient runtime).
pub(crate) fn test_lock(handle: Option<tokio::runtime::Handle>) -> ResourceLock {
	test_lock_on(offline_client(), handle)
}

/// A valid lock held by `client`, whose release, when dropped, goes to `handle` (or the ambient
/// runtime).
pub(crate) fn test_lock_on(
	client: Arc<AuthClient>,
	handle: Option<tokio::runtime::Handle>,
) -> ResourceLock {
	ResourceLock {
		lease: Arc::new(SharedLease {
			uuid: Uuid::new_v4(),
			client,
			resource: "test-resource".to_string(),
			valid: tokio::sync::watch::Sender::new(true),
			confirmed: tokio::sync::watch::Sender::new(Confirmed::now()),
			confirming: tokio::sync::Mutex::new(None),
			confirmations: AtomicU64::new(0),
			handle,
		}),
	}
}

/// A runtime only the test drives: a lock made in it ([`held_lock`]) queues its release there
/// when it drops, and the release is never sent unless the test drives it.
pub(crate) fn undriven_runtime() -> Runtime {
	Builder::new_current_thread().enable_all().build().unwrap()
}

/// A valid lock whose release, when dropped, goes to `rt`.
pub(crate) fn held_lock(rt: &Runtime) -> Arc<ResourceLock> {
	rt.block_on(async { Arc::new(test_lock(Handle::try_current().ok())) })
}

/// Marks `lock` lost, as its keep-alive does when the server refuses the lease, without the
/// release [`SharedLease::mark_lost`] sends.
pub(crate) fn lose(lock: &ResourceLock) {
	lock.lease.valid.send_replace(false);
}
