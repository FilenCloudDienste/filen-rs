//! How a sync pair reconciles its two sides: direction + deletion policy.

/// The sync direction and deletion policy for one folder pair.
///
/// The five modes are two orthogonal knobs — which side's changes flow where, and whether a
/// deletion on the source side is mirrored to the destination. A "backup" mode is a one-way mirror
/// that never deletes on its destination, so the destination only ever accumulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
	/// Local is authoritative: push local creates/edits/moves up and mirror local deletions to
	/// the remote.
	LocalToRemote,
	/// Remote is authoritative: pull remote changes down and mirror remote deletions locally.
	RemoteToLocal,
	/// Bidirectional. Changes flow both ways; a genuine both-sides-changed conflict is surfaced to
	/// the caller rather than resolved silently.
	TwoWay,
	/// Like [`LocalToRemote`](Self::LocalToRemote) but additive: local changes are pushed up, but
	/// a local deletion is NOT mirrored — the remote backup retains it.
	LocalBackup,
	/// Like [`RemoteToLocal`](Self::RemoteToLocal) but additive: remote changes are pulled down,
	/// but a remote deletion is NOT mirrored — the local backup retains it.
	RemoteBackup,
}

/// What a [`reconfigure_pair`](super::SyncEngine::reconfigure_pair) does with the divergence the
/// OLD mode deliberately left standing — the backup destination's copies of items the source has
/// since deleted.
///
/// It only ever matters when the old mode was additive and the new one is not: a backup mode never
/// propagates a source deletion, so its destination accumulates copies the mirror modes would
/// remove on their next pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backlog {
	/// Reconcile the two sides as they are, under the new rules. Every deletion the old mode left
	/// standing is pending from the next pass and propagates at once (mass-delete-guard screened).
	Propagate,
	/// Adopt the destination's standing copies first: every path the SOURCE has nothing at, but the
	/// destination does, is re-seeded into the baseline from the destination's current state before
	/// the switch takes effect.
	///
	/// In a one-way mirror those copies then count as intended — the next pass neither deletes them
	/// nor pushes them back to the source, until the source has something at that path again — and
	/// that holds whether or not the pair ever tracked the path, so a file another client wrote
	/// straight to the destination survives the switch alongside the pair's own standing backlog.
	/// In [`TwoWay`](SyncMode::TwoWay) only the paths the pair tracked are re-seeded, from whichever
	/// side still holds them; an untracked copy needs no row, because it already flows to the other
	/// side under the ordinary rules.
	///
	/// Because it is a judgement about what one side no longer has, the switch is REFUSED on the
	/// same evidence a pass holds its deletions on — a name collision, an incomplete local scan, an
	/// unconverged or wholly empty remote view — leaving the mode unchanged for the caller to retry.
	AdoptDestination,
}

impl SyncMode {
	/// Whether local-side changes flow to the remote.
	pub(crate) fn pushes(self) -> bool {
		matches!(self, Self::LocalToRemote | Self::TwoWay | Self::LocalBackup)
	}

	/// Whether remote-side changes flow to the local tree.
	pub(crate) fn pulls(self) -> bool {
		matches!(
			self,
			Self::RemoteToLocal | Self::TwoWay | Self::RemoteBackup
		)
	}

	/// Whether a deletion on the source side is propagated to the destination. Mirror modes
	/// propagate; backup modes ([`LocalBackup`](Self::LocalBackup) /
	/// [`RemoteBackup`](Self::RemoteBackup)) do not, so their destination accumulates.
	pub(crate) fn propagates_deletes(self) -> bool {
		matches!(
			self,
			Self::LocalToRemote | Self::RemoteToLocal | Self::TwoWay
		)
	}

	/// Whether a true both-sides-changed conflict can arise (only bidirectional sync).
	// Companion predicate to `pushes`/`pulls`/`propagates_deletes`; exercised by the mode unit tests.
	#[allow(dead_code)]
	pub(crate) fn can_conflict(self) -> bool {
		matches!(self, Self::TwoWay)
	}

	/// Stable integer encoding for the `sync_pairs.mode` column.
	pub(crate) fn as_i64(self) -> i64 {
		match self {
			Self::LocalToRemote => 0,
			Self::RemoteToLocal => 1,
			Self::TwoWay => 2,
			Self::LocalBackup => 3,
			Self::RemoteBackup => 4,
		}
	}

	/// Decode the [`as_i64`](Self::as_i64) encoding; `None` for an unknown value.
	pub(crate) fn from_i64(value: i64) -> Option<Self> {
		Some(match value {
			0 => Self::LocalToRemote,
			1 => Self::RemoteToLocal,
			2 => Self::TwoWay,
			3 => Self::LocalBackup,
			4 => Self::RemoteBackup,
			_ => return None,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mode_encoding_round_trips() {
		for mode in [
			SyncMode::LocalToRemote,
			SyncMode::RemoteToLocal,
			SyncMode::TwoWay,
			SyncMode::LocalBackup,
			SyncMode::RemoteBackup,
		] {
			assert_eq!(SyncMode::from_i64(mode.as_i64()), Some(mode));
		}
		assert_eq!(SyncMode::from_i64(99), None);
	}

	#[test]
	fn backup_modes_are_one_way_mirrors_that_never_delete() {
		// LocalBackup: pushes up, never pulls, never deletes.
		assert!(SyncMode::LocalBackup.pushes());
		assert!(!SyncMode::LocalBackup.pulls());
		assert!(!SyncMode::LocalBackup.propagates_deletes());
		// RemoteBackup: the mirror image.
		assert!(SyncMode::RemoteBackup.pulls());
		assert!(!SyncMode::RemoteBackup.pushes());
		assert!(!SyncMode::RemoteBackup.propagates_deletes());
		// Mirror modes propagate deletes; only TwoWay can conflict.
		assert!(SyncMode::LocalToRemote.propagates_deletes());
		assert!(SyncMode::RemoteToLocal.propagates_deletes());
		assert!(SyncMode::TwoWay.can_conflict());
		assert!(!SyncMode::LocalToRemote.can_conflict());
	}
}
