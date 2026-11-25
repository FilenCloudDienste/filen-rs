//! The name resolution of the SDK's HTTP client when a connect timeout is set, and of the
//! websocket's connection to a proxy always: the system resolver, keeping only as many addresses
//! as the connect timeout leaves each attempt [`CONNECT_ATTEMPT_FLOOR`] for, or every address
//! without a connect timeout.

use std::{
	future::Future,
	io,
	net::{SocketAddr, ToSocketAddrs},
	num::NonZeroUsize,
	time::Duration,
};

use tokio::task::JoinHandle;

/// The least share of the connect timeout each TCP connect attempt gets.
///
/// hyper-util's `HttpConnector`, which both reqwest and the websocket's proxy connection connect
/// through, tries a host's addresses one after the other and divides the connect timeout evenly
/// among them: per address family, as it splits the addresses into the family of the first one
/// and the other (happy eyeballs). The more addresses a name resolves to, the shorter each
/// attempt. On the 2026-11-25 nightly, `egest.filen.io` resolved to 8 IPv4 addresses on a macOS
/// runner, so each attempt had 10 s / 8 = 1.25 s: the first timed out after 1.255 s, the other
/// seven failed within 1 ms, and only the first error, the timeout, was reported. A connect
/// timeout is not retried (`is_pre_send_connect_failure`), so the download failed.
///
/// [`CappedResolver`] hands on at most `connect timeout / CONNECT_ATTEMPT_FLOOR` addresses of each
/// family, and always at least one, so each attempt's own deadline is at least this unless the
/// whole timeout is shorter. reqwest also bounds the whole connect, name resolution and TLS
/// included, by the same timeout, and hyper-util starts the second family only after its happy
/// eyeballs delay of 300 ms, so that overall deadline can still cut the last attempt of a family
/// short. 2.5 s leaves an attempt room for a SYN retransmitted after the usual initial
/// retransmission timeout of 1 s, with more than a second to spare for its answer, where 1.25 s
/// left a quarter of a second; and at the default 10 s timeout it still tries 4 addresses of a
/// family, an exact multiple, so failing over to another server stays possible among those 4. The
/// addresses past the cap are never tried, even when the kept ones are refused at once, where
/// such fast failures used to fall through to them. They are the ones the system resolver lists
/// last, so a name served round-robin still spreads its connections over all of them.
pub(crate) const CONNECT_ATTEMPT_FLOOR: Duration = Duration::from_millis(2500);

/// How a host name becomes addresses: [`SystemLookup`] outside tests.
pub(crate) trait Lookup: Clone + Send + Sync + 'static {
	/// The addresses `host` resolves to, in the resolver's order, with port 0 (the connector puts
	/// in the port).
	fn lookup(
		&self,
		host: Box<str>,
	) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'static;
}

/// The system resolver (`getaddrinfo`) on a blocking thread, as reqwest's default resolver and
/// hyper-util's `GaiResolver` call it, and like `GaiResolver` aborting a lookup still queued for a
/// blocking thread when the caller gives up on it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SystemLookup;

impl Lookup for SystemLookup {
	fn lookup(
		&self,
		host: Box<str>,
	) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'static {
		// Started on the call, as hyper-util's resolver starts it.
		let mut lookup = AbortOnDrop(tokio::task::spawn_blocking(move || {
			(&*host, 0).to_socket_addrs().map(Iterator::collect)
		}));
		async move {
			match (&mut lookup.0).await {
				Ok(addrs) => addrs,
				// What hyper-util's resolver answers for a lookup the runtime cancelled. A panic in
				// the lookup, which hyper-util re-raises, is an error here instead.
				Err(e) if e.is_cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, e)),
				Err(e) => Err(io::Error::other(e)),
			}
		}
	}
}

/// A blocking task aborted when its handle is dropped, so a lookup nobody waits for any more does
/// not take a blocking thread once one is free. One already running finishes regardless.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// A resolver that keeps at most `per_family` addresses of each address family, in the order
/// `lookup` returns them (see [`CONNECT_ATTEMPT_FLOOR`]).
#[derive(Clone, Debug)]
pub(crate) struct CappedResolver<L> {
	lookup: L,
	/// `None` keeps every address: without a connect timeout no attempt is cut short.
	per_family: Option<NonZeroUsize>,
}

impl<L: Lookup> CappedResolver<L> {
	/// A resolver for connections bounded by `connect_timeout`.
	pub(crate) fn new(lookup: L, connect_timeout: Option<Duration>) -> Self {
		Self {
			lookup,
			per_family: connect_timeout.map(attempts_per_family),
		}
	}

	/// The addresses `host` resolves to, capped.
	fn resolve_capped(
		&self,
		host: Box<str>,
	) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'static {
		let lookup = self.lookup.lookup(host);
		let per_family = self.per_family;
		async move {
			let addrs = lookup.await?;
			Ok(match per_family {
				Some(per_family) => cap_per_family(addrs, per_family),
				None => addrs,
			})
		}
	}
}

impl<L: Lookup> reqwest::dns::Resolve for CappedResolver<L> {
	fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
		let resolving = self.resolve_capped(name.as_str().into());
		Box::pin(async move {
			resolving
				.await
				.map(|addrs| Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
				.map_err(Into::into)
		})
	}
}

/// The resolver of a hyper-util `HttpConnector`, which the websocket reaches a proxy with.
#[cfg(platform_net)]
impl<L: Lookup> tower::Service<hyper_util::client::legacy::connect::dns::Name>
	for CappedResolver<L>
{
	type Response = std::vec::IntoIter<SocketAddr>;
	type Error = io::Error;
	type Future = std::pin::Pin<
		Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
	>;

	fn poll_ready(
		&mut self,
		_cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		std::task::Poll::Ready(Ok(()))
	}

	fn call(&mut self, name: hyper_util::client::legacy::connect::dns::Name) -> Self::Future {
		let resolving = self.resolve_capped(name.as_str().into());
		Box::pin(async move { resolving.await.map(Vec::into_iter) })
	}
}

/// How many attempts of [`CONNECT_ATTEMPT_FLOOR`] fit into `connect_timeout`, and at least one.
fn attempts_per_family(connect_timeout: Duration) -> NonZeroUsize {
	let fits = connect_timeout.as_nanos() / CONNECT_ATTEMPT_FLOOR.as_nanos();
	NonZeroUsize::new(usize::try_from(fits).unwrap_or(usize::MAX)).unwrap_or(NonZeroUsize::MIN)
}

/// `addrs` without the IPv4 addresses after the first `per_family` and the IPv6 addresses after
/// the first `per_family`, in their order.
fn cap_per_family(addrs: Vec<SocketAddr>, per_family: NonZeroUsize) -> Vec<SocketAddr> {
	let (mut v4, mut v6) = (0usize, 0usize);
	addrs
		.into_iter()
		.filter(|addr| {
			let kept = if addr.is_ipv4() { &mut v4 } else { &mut v6 };
			if *kept < per_family.get() {
				*kept += 1;
				true
			} else {
				false
			}
		})
		.collect()
}

#[cfg(test)]
mod tests;
