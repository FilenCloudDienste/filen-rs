use std::{
	error::Error as _,
	future::Future,
	io,
	net::{Ipv4Addr, Ipv6Addr, SocketAddr},
	num::NonZeroUsize,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

#[cfg(platform_net)]
use hyper_util::client::legacy::connect::dns::Name;
use tokio::net::{TcpListener, TcpSocket};
#[cfg(platform_net)]
use tower::Service as _;

use super::{CONNECT_ATTEMPT_FLOOR, CappedResolver, Lookup, attempts_per_family, cap_per_family};
use crate::auth::http::{
	ClientConfig, DEFAULT_CONNECT_TIMEOUT,
	test_support::{read_head, respond_with},
};

/// A lookup that answers every host with the same addresses.
#[derive(Clone)]
struct FixedLookup(Vec<SocketAddr>);

impl Lookup for FixedLookup {
	fn lookup(
		&self,
		_host: Box<str>,
	) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'static {
		std::future::ready(Ok(self.0.clone()))
	}
}

fn v4(last: u8) -> SocketAddr {
	SocketAddr::from((Ipv4Addr::new(192, 0, 2, last), 0))
}

fn v6(last: u16) -> SocketAddr {
	SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last), 0))
}

fn per_family(n: usize) -> NonZeroUsize {
	NonZeroUsize::new(n).unwrap()
}

#[test]
fn the_default_connect_timeout_leaves_room_for_four_attempts_per_family() {
	assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs(10));
	assert_eq!(CONNECT_ATTEMPT_FLOOR, Duration::from_millis(2500));
	assert_eq!(attempts_per_family(DEFAULT_CONNECT_TIMEOUT), per_family(4));
}

#[test]
fn an_exact_multiple_of_the_floor_counts_every_attempt_it_fits() {
	assert_eq!(attempts_per_family(CONNECT_ATTEMPT_FLOOR), per_family(1));
	assert_eq!(
		attempts_per_family(CONNECT_ATTEMPT_FLOOR * 2),
		per_family(2)
	);
	assert_eq!(
		attempts_per_family(CONNECT_ATTEMPT_FLOOR * 3),
		per_family(3)
	);
}

#[test]
fn a_timeout_just_short_of_a_multiple_rounds_down() {
	let just_short = CONNECT_ATTEMPT_FLOOR * 2 - Duration::from_nanos(1);
	assert_eq!(attempts_per_family(just_short), per_family(1));
}

#[test]
fn a_timeout_below_the_floor_still_allows_one_attempt() {
	assert_eq!(attempts_per_family(Duration::from_secs(1)), per_family(1));
	assert_eq!(attempts_per_family(Duration::ZERO), per_family(1));
}

/// `Duration::MAX` over 2.5 s, rounded down, which fits a 64-bit `usize`.
#[cfg(target_pointer_width = "64")]
#[test]
fn the_longest_timeout_does_not_overflow() {
	assert_eq!(
		attempts_per_family(Duration::MAX).get(),
		7_378_697_629_483_820_646
	);
}

#[test]
fn eight_ipv4_addresses_are_cut_to_the_first_four_in_order() {
	let addrs = (1..=8).map(v4).collect();
	assert_eq!(
		cap_per_family(addrs, per_family(4)),
		vec![v4(1), v4(2), v4(3), v4(4)]
	);
}

#[test]
fn each_family_keeps_its_own_first_addresses_in_their_interleaved_order() {
	let addrs = vec![v6(1), v4(1), v6(2), v4(2), v6(3), v4(3), v4(4), v6(4)];
	assert_eq!(
		cap_per_family(addrs, per_family(2)),
		vec![v6(1), v4(1), v6(2), v4(2)]
	);
}

#[test]
fn a_family_under_the_cap_keeps_all_its_addresses() {
	let addrs = vec![v4(1), v6(1), v4(2), v4(3), v4(4)];
	assert_eq!(
		cap_per_family(addrs, per_family(2)),
		vec![v4(1), v6(1), v4(2)]
	);
}

#[test]
fn a_cap_of_one_keeps_the_first_address_of_each_family() {
	let addrs = vec![v4(1), v4(2), v6(1), v6(2)];
	assert_eq!(cap_per_family(addrs, per_family(1)), vec![v4(1), v6(1)]);
}

#[test]
fn no_addresses_stay_no_addresses() {
	assert!(cap_per_family(Vec::new(), per_family(1)).is_empty());
}

#[tokio::test]
async fn without_a_connect_timeout_every_address_is_kept() {
	let addrs: Vec<_> = (1..=8).map(v4).chain((1..=3).map(v6)).collect();
	let resolver = CappedResolver::new(FixedLookup(addrs.clone()), None);
	assert_eq!(resolver.resolve_capped("host".into()).await.unwrap(), addrs);
}

#[tokio::test]
async fn with_a_connect_timeout_the_resolved_addresses_are_capped() {
	let addrs = (1..=8).map(v4).collect();
	let resolver = CappedResolver::new(FixedLookup(addrs), Some(DEFAULT_CONNECT_TIMEOUT));
	assert_eq!(
		resolver.resolve_capped("host".into()).await.unwrap(),
		vec![v4(1), v4(2), v4(3), v4(4)]
	);
}

#[cfg(platform_net)]
#[tokio::test]
async fn the_proxy_connectors_resolver_caps_the_resolved_addresses() {
	let mut resolver = CappedResolver::new(
		FixedLookup((1..=8).map(v4).collect()),
		Some(DEFAULT_CONNECT_TIMEOUT),
	);
	std::future::poll_fn(|cx| resolver.poll_ready(cx))
		.await
		.unwrap();
	let addrs: Vec<_> = resolver
		.call("host".parse::<Name>().unwrap())
		.await
		.unwrap()
		.collect();
	assert_eq!(addrs, vec![v4(1), v4(2), v4(3), v4(4)]);
}

/// A host name no resolver resolves (RFC 6761), so a request to it gets through only when the
/// client asks the test's lookup.
const UNRESOLVABLE: &str = "http://resolver-test.invalid/";

/// A client configured as [`ClientConfig::build_http`]'s, without its TLS configuration and
/// without proxies, that resolves every host name to `addrs`.
fn client_resolving_to(connect_timeout: Duration, addrs: Vec<SocketAddr>) -> reqwest::Client {
	ClientConfig::default()
		.with_connect_timeout(Some(connect_timeout))
		.reqwest_builder(FixedLookup(addrs))
		.no_proxy()
		.build()
		.unwrap()
}

/// A loopback server answering every request with `ok`, and the number of connections it has
/// accepted.
async fn answering_server() -> (SocketAddr, Arc<AtomicUsize>) {
	let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
	let addr = listener.local_addr().unwrap();
	let accepted = Arc::new(AtomicUsize::new(0));
	let counter = Arc::clone(&accepted);
	tokio::spawn(async move {
		loop {
			let (mut socket, _) = listener.accept().await.unwrap();
			counter.fetch_add(1, Ordering::SeqCst);
			tokio::spawn(async move {
				read_head(&mut socket).await;
				respond_with(&mut socket, "ok").await;
			});
		}
	});
	(addr, accepted)
}

/// The kind of the first I/O error in `err`'s chain of sources.
fn io_error_kind(err: &reqwest::Error) -> Option<io::ErrorKind> {
	std::iter::successors(err.source(), |&e| e.source())
		.find_map(|e| e.downcast_ref::<io::Error>())
		.map(io::Error::kind)
}

#[tokio::test]
async fn the_client_connects_to_the_addresses_its_resolver_returns() {
	let (answering, _) = answering_server().await;
	let client = client_resolving_to(DEFAULT_CONNECT_TIMEOUT, vec![answering]);
	let body = client
		.get(UNRESOLVABLE)
		.send()
		.await
		.unwrap()
		.text()
		.await
		.unwrap();
	assert_eq!(body, "ok");
}

/// With room for one attempt only, the client never tries the second address, which would
/// answer. The first is a loopback port held bound without listening, so nothing else can take
/// it: Linux refuses a connection to it, macOS lets it time out, and either way it fails, unlike a
/// failed name resolution. The URL names no port, so each address keeps its own. The timeout is
/// below [`CONNECT_ATTEMPT_FLOOR`], so the macOS timeout is short; uncapped, each address would get
/// half of it, and the second, a loopback connect, would be accepted.
#[tokio::test]
async fn a_timeout_with_room_for_one_attempt_tries_only_the_first_address() {
	let unanswered = TcpSocket::new_v4().unwrap();
	unanswered.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
	let (answering, accepted) = answering_server().await;
	let client = client_resolving_to(
		Duration::from_millis(200),
		vec![unanswered.local_addr().unwrap(), answering],
	);
	let err = client.get(UNRESOLVABLE).send().await.unwrap_err();
	assert!(err.is_connect(), "expected a connect error, got {err:?}");
	assert!(
		err.is_timeout() || io_error_kind(&err) == Some(io::ErrorKind::ConnectionRefused),
		"expected the first address to time out or refuse, got {err:?}"
	);
	assert_eq!(accepted.load(Ordering::SeqCst), 0);
}
