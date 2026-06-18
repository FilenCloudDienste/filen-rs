//! Shared harness for the grouped sync-engine test suite.
//!
//! The suite is ONE integration-test binary (`sync_suite`) with one module per category (see
//! `sync_suite.rs`). Every category module does `use crate::harness::*;` for the common scaffolding
//! below: fresh local dirs, byte-exact tree comparison (ignoring the engine's quarantine bin),
//! single-client and two-client setups, and a deterministic multi-pass convergence helper.
//!
//! These are LIVE tests against the real backend (via `test_utils::RESOURCES`); each test scopes to
//! its own fresh remote dir (auto-cleaned) so they stay account-size-independent.
#![allow(dead_code)]

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	time::Duration,
};

use filen_sdk_rs::fs::HasUUID;
use filen_sdk_rs::sync_engine::{SyncEngine, SyncMode, SyncReport};
use uuid::Uuid;

use crate::helpers::*;

/// A fresh, unique local sync-root temp directory.
pub fn fresh_local_dir(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("sync_suite_{tag}_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Deterministic content for a relative path (the path bytes), so a pull side can be checked to
/// have reproduced exactly the same bytes.
pub fn content_for(rel: &str) -> Vec<u8> {
	rel.as_bytes().to_vec()
}

/// Write a file (creating parent dirs) under `root`.
pub fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
	let path = root.join(rel);
	std::fs::create_dir_all(path.parent().unwrap()).unwrap();
	std::fs::write(&path, bytes).unwrap();
}

/// True if the file at `rel` under `root` exists and its bytes equal `expected`.
pub fn read_eq(root: &Path, rel: &str, expected: &[u8]) -> bool {
	std::fs::read(root.join(rel)).is_ok_and(|b| b == expected)
}

/// Rename/move `from` -> `to` under `root` (creating the destination's parent dirs).
pub fn move_file(root: &Path, from: &str, to: &str) {
	let dst = root.join(to);
	std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
	std::fs::rename(root.join(from), dst).unwrap();
}

/// One entry in a walked tree: relative path -> (is_dir, file_len, content_bytes).
pub type TreeMap = BTreeMap<String, (bool, u64, Vec<u8>)>;

/// Recursively walk `root` into a sorted relative-path map. Excludes the engine's local quarantine
/// bin (`.filen-sync-trash`, top-level): it is per-side recovery state, not synced content, so two
/// genuinely-converged trees must compare equal modulo it.
pub fn walk_tree(root: &Path) -> TreeMap {
	let mut map = TreeMap::new();
	walk_into(root, root, &mut map);
	map
}

fn walk_into(root: &Path, dir: &Path, map: &mut TreeMap) {
	let mut entries: Vec<_> = match std::fs::read_dir(dir) {
		Ok(rd) => rd.map(|e| e.unwrap()).collect(),
		Err(_) => return,
	};
	entries.sort_by_key(|e| e.file_name());
	for entry in entries {
		let path = entry.path();
		let rel = path
			.strip_prefix(root)
			.unwrap()
			.to_string_lossy()
			.replace('\\', "/");
		if rel == ".filen-sync-trash" {
			continue;
		}
		let ft = entry.file_type().unwrap();
		if ft.is_dir() {
			map.insert(rel, (true, 0, Vec::new()));
			walk_into(root, &path, map);
		} else if ft.is_file() {
			let content = std::fs::read(&path).unwrap();
			map.insert(rel, (false, content.len() as u64, content));
		}
	}
}

/// True once the two local trees are byte-for-byte identical (quarantine bin ignored).
pub fn trees_equal(a: &Path, b: &Path) -> bool {
	walk_tree(a) == walk_tree(b)
}

/// Recursively scan `root` — INCLUDING the engine's quarantine bin — for any regular file whose bytes
/// equal `needle`. Use this (rather than a path-specific [`read_eq`]) to assert that data is
/// recoverable SOMEWHERE in the tree: e.g. when a remote delete races a local move, the relocated
/// copy is preserved under the quarantine bin rather than at its destination path.
pub fn bytes_recoverable_anywhere(root: &Path, needle: &[u8]) -> bool {
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let Ok(rd) = std::fs::read_dir(&dir) else {
			continue;
		};
		for entry in rd.flatten() {
			let Ok(ft) = entry.file_type() else {
				continue;
			};
			if ft.is_dir() {
				stack.push(entry.path());
			} else if ft.is_file() && std::fs::read(entry.path()).is_ok_and(|b| b == needle) {
				return true;
			}
		}
	}
	false
}

/// Assert two local trees are identical, naming the first divergence.
pub fn assert_trees_identical(a: &Path, b: &Path, label_a: &str, label_b: &str) {
	let ta = walk_tree(a);
	let tb = walk_tree(b);
	for key in ta.keys() {
		assert!(
			tb.contains_key(key),
			"path {key:?} present in {label_a} but missing in {label_b}"
		);
	}
	for key in tb.keys() {
		assert!(
			ta.contains_key(key),
			"path {key:?} present in {label_b} but missing in {label_a}"
		);
	}
	for (key, av) in &ta {
		assert_eq!(
			av,
			tb.get(key).unwrap(),
			"path {key:?} differs ({label_a} vs {label_b})"
		);
	}
	assert_eq!(ta.len(), tb.len(), "tree sizes differ");
}

/// A single-client sync setup: one engine on its own derived cache, a fresh local dir, and a fresh
/// auto-cleaned remote dir already converged into the cache.
pub struct SingleClient {
	pub resources: test_utils::TestResources,
	pub cache: TestCache,
	pub engine: SyncEngine,
	pub pair: i64,
	pub local: PathBuf,
	pub remote: Uuid,
}

impl SingleClient {
	/// One sync pass; panics on engine error (per-action errors are still in the returned report).
	pub async fn sync(&self) -> SyncReport {
		self.engine.sync_once(self.pair).await.expect("sync_once")
	}

	pub fn cleanup(&self) {
		std::fs::remove_dir_all(&self.local).ok();
	}
}

/// Build a single-client setup in `mode`, with the (empty) remote converged into the cache.
pub async fn single_client(mode: SyncMode) -> SingleClient {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("sc");
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine.add_pair(local.clone(), remote, mode).await.unwrap();
	SingleClient {
		resources,
		cache,
		engine,
		pair,
		local,
		remote,
	}
}

/// Two independent client+cache+engine stacks syncing the SAME remote dir, each with its own local
/// dir — for two-way / convergence / conflict / contention tests.
pub struct TwoClients {
	pub resources: test_utils::TestResources,
	pub cache_a: TestCache,
	pub cache_b: TestCache,
	pub engine_a: SyncEngine,
	pub engine_b: SyncEngine,
	pub pair_a: i64,
	pub pair_b: i64,
	pub local_a: PathBuf,
	pub local_b: PathBuf,
	pub remote: Uuid,
}

impl TwoClients {
	pub fn cleanup(&self) {
		std::fs::remove_dir_all(&self.local_a).ok();
		std::fs::remove_dir_all(&self.local_b).ok();
	}
}

/// Build two clients on one shared remote dir, both in `mode`.
pub async fn two_clients(mode: SyncMode) -> TwoClients {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();
	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;
	let local_a = fresh_local_dir("a");
	let local_b = fresh_local_dir("b");
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, mode)
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, mode)
		.await
		.unwrap();
	TwoClients {
		resources,
		cache_a,
		cache_b,
		engine_a,
		engine_b,
		pair_a,
		pair_b,
		local_a,
		local_b,
		remote,
	}
}

/// The order in which two clients' passes run within a convergence round.
#[derive(Clone, Copy, Debug)]
pub enum Order {
	AFirst,
	BFirst,
	Concurrent,
}

/// Run one sync round (one pass per client) in the given order; returns both reports.
pub async fn sync_round(
	ea: &SyncEngine,
	pa: i64,
	eb: &SyncEngine,
	pb: i64,
	order: Order,
) -> (SyncReport, SyncReport) {
	match order {
		Order::AFirst => {
			let ra = ea.sync_once(pa).await.expect("engine A sync_once");
			let rb = eb.sync_once(pb).await.expect("engine B sync_once");
			(ra, rb)
		}
		Order::BFirst => {
			let rb = eb.sync_once(pb).await.expect("engine B sync_once");
			let ra = ea.sync_once(pa).await.expect("engine A sync_once");
			(ra, rb)
		}
		Order::Concurrent => {
			let (ra, rb) = tokio::join!(ea.sync_once(pa), eb.sync_once(pb));
			(
				ra.expect("engine A sync_once"),
				rb.expect("engine B sync_once"),
			)
		}
	}
}

/// Run sync rounds (in `order`) on two clients until `done` holds, panicking after a bound. Each
/// pass must be error-free; surfaced conflicts are accumulated into `conflicts`. A short settle
/// between rounds lets each client's cache observe the other's just-committed remote writes.
#[allow(clippy::too_many_arguments)]
pub async fn converge(
	ea: &SyncEngine,
	pa: i64,
	eb: &SyncEngine,
	pb: i64,
	order: Order,
	conflicts: &mut std::collections::BTreeSet<String>,
	label: &str,
	done: impl Fn() -> bool,
) {
	const MAX_ROUNDS: usize = 20;
	for _ in 0..MAX_ROUNDS {
		let (ra, rb) = sync_round(ea, pa, eb, pb, order).await;
		assert!(
			ra.errors.is_empty(),
			"{label}: engine A errors {:?}",
			ra.errors
		);
		assert!(
			rb.errors.is_empty(),
			"{label}: engine B errors {:?}",
			rb.errors
		);
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
		}
		if done() {
			return;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	panic!("{label}: did not converge within {MAX_ROUNDS} rounds (order {order:?})");
}
