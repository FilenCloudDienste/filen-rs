//! The sync root's watcher: `notify`'s recommended watcher on the root, recursively
//! (`ReadDirectoryChangesW` on Windows), its events turned into paths for the local engine to look
//! at. Built on every host, so the translation is tested off Windows.
//!
//! An event is never a verdict (the sync engine's rule): the provider's own hydrations, converts
//! and populations raise the same events as a user's writes (measured), and the engine tells them
//! apart by the state it reads. New and ordinary items, saves that replace a placeholder and moves
//! of ordinary items reach the provider through this watcher alone, as the platform raises no
//! callback for them (measured).

use std::{
	path::{Component, Path, PathBuf},
	sync::Arc,
};

use filen_sdk_rs::sync_engine::{LocalEventScope, local_event_scope};
use notify::{
	Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
	event::{ModifyKind, RenameMode},
};
use tracing::warn;

use crate::{
	drive::Drive,
	local::LocalEngine,
	names::PlaceholderName,
	os::{LocalEvent, LocalFs},
	root_path::RootPath,
};

/// Why the watcher did not start.
#[derive(Debug, thiserror::Error)]
#[error("watching the sync root failed")]
pub struct WatchError(#[source] notify::Error);

/// What a watcher hands on: a path to look at, or its own failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSignal {
	/// A path, or the whole root, to look at.
	Event(LocalEvent),
	/// The watcher failed, with `notify`'s message: it may have stopped watching, so whoever
	/// started it starts a new one and walks the root.
	Failed(String),
}

/// Where a watcher hands its signals, from `notify`'s thread.
pub type WatchSink = Arc<dyn Fn(WatchSignal) + Send + Sync>;

/// A running watcher of the sync root, which hands every change to the local engine. Dropping it
/// stops it: drop it before [`LocalEngine::shutdown`].
pub struct LocalWatcher {
	_watcher: RecommendedWatcher,
}

impl LocalWatcher {
	/// Watches `root`, the sync root's full path, recursively, handing `engine` a path to look at
	/// for every event below it, and a walk of the whole root for a lost event or a watcher error.
	pub fn start<D: Drive, L: LocalFs>(
		root: &Path,
		engine: Arc<LocalEngine<D, L>>,
	) -> Result<Self, WatchError> {
		Self::start_signalling(
			root,
			Arc::new(move |signal| match signal {
				WatchSignal::Event(event) => engine.on_local_event(event),
				WatchSignal::Failed(error) => {
					warn!(%error, "the sync root's watcher failed; walking the root");
					engine.on_local_event(LocalEvent::Rescan);
				}
			}),
		)
	}

	/// Watches `root`, the sync root's full path, recursively, handing `sink` a path to look at for
	/// every event below it, a walk of the whole root for a lost event, and the watcher's failure
	/// as it is.
	pub fn start_signalling(root: &Path, sink: WatchSink) -> Result<Self, WatchError> {
		let watched = root.to_path_buf();
		let mut watcher =
			notify::recommended_watcher(move |result: notify::Result<Event>| match result {
				Ok(event) => {
					for event in local_events(&watched, Ok(event)) {
						sink(WatchSignal::Event(event));
					}
				}
				Err(error) => sink(WatchSignal::Failed(error.to_string())),
			})
			.map_err(WatchError)?;
		watcher
			.watch(root, RecursiveMode::Recursive)
			.map_err(WatchError)?;
		Ok(Self { _watcher: watcher })
	}
}

/// Where a path the watcher reports lies.
#[derive(Debug, PartialEq, Eq)]
enum Located {
	/// The sync root itself.
	Root,
	/// Below the root: the item, or, when a component is a name the core cannot name, the deepest
	/// directory above it the core can, whose job reports the item skipped.
	Below(RootPath),
	/// Outside the root: the watcher reports what it should not.
	Outside,
}

/// Where `path` lies relative to `root`, the path the watcher was started on.
fn locate(root: &Path, path: &Path) -> Located {
	let Ok(relative) = path.strip_prefix(root) else {
		return Located::Outside;
	};
	let mut names = Vec::new();
	for component in relative.components() {
		let Component::Normal(name) = component else {
			return Located::Outside;
		};
		match name.to_str().map(PlaceholderName::parse) {
			Some(Ok(name)) => names.push(name),
			_ => break,
		}
	}
	if names.is_empty() && relative.as_os_str().is_empty() {
		return Located::Root;
	}
	Located::Below(
		names
			.into_iter()
			.fold(RootPath::root(), |path, name| path.join(name)),
	)
}

/// The paths of `event` below `root`, or `None` when one lies outside it.
fn below(root: &Path, paths: &[PathBuf]) -> Option<Vec<Option<RootPath>>> {
	paths
		.iter()
		.map(|path| match locate(root, path) {
			Located::Root => Some(None),
			Located::Below(path) => Some(Some(path)),
			Located::Outside => None,
		})
		.collect()
}

/// What the watcher's `result` asks the local engine to look at, the watcher started on `root`.
/// Each event is read as the sync engine reads it ([`local_event_scope`]): a lost event or a change
/// to the root itself asks for a walk of the whole root, as do a watcher error and a path outside
/// the root; a read is nothing to look at.
///
/// `notify`'s Windows backend does not report a lost event: it ignores a `ReadDirectoryChangesW`
/// buffer overflow's empty result and stops watching, silently, on an error it does not know. The
/// engine's safety-net walk is what finds such changes there.
pub(crate) fn local_events(root: &Path, result: notify::Result<Event>) -> Vec<LocalEvent> {
	let event = match result {
		Ok(event) => event,
		Err(error) => {
			warn!(%error, "the sync root's watcher failed; walking the root");
			return vec![LocalEvent::Rescan];
		}
	};
	match local_event_scope(&event) {
		LocalEventScope::Collapse(_) => return vec![LocalEvent::Rescan],
		LocalEventScope::Ignore => return Vec::new(),
		LocalEventScope::Paths => {}
	}
	let Some(located) = below(root, &event.paths) else {
		return vec![LocalEvent::Rescan];
	};
	let moves_root = matches!(
		event.kind,
		EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
	);
	if moves_root && located.iter().any(Option::is_none) {
		// The root itself was removed or renamed.
		return vec![LocalEvent::Rescan];
	}
	let located: Vec<RootPath> = located.into_iter().flatten().collect();
	match (&event.kind, located.as_slice()) {
		(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), [from, to]) => {
			vec![LocalEvent::Renamed {
				from: from.clone(),
				to: to.clone(),
			}]
		}
		(EventKind::Create(_), _) => located.into_iter().map(LocalEvent::Created).collect(),
		(EventKind::Remove(_), _) => located.into_iter().map(LocalEvent::Removed).collect(),
		_ => located.into_iter().map(LocalEvent::Modified).collect(),
	}
}

#[cfg(test)]
mod tests;
