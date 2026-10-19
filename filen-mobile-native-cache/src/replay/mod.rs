//! Closing a drive gap by REPLAYING `v3/user/events` instead of re-listing everything.
//!
//! The gap pass this fronts ([`crate::live::gap_check`]) re-lists every materialized
//! container — ~3,000 `dir/content` round trips on a real account, tens of seconds on wifi and
//! minutes on cellular, and it costs the same whether the gap is 12,000 events or 8. The event
//! log can say WHICH containers moved, so the common case becomes a listing or two.
//!
//! This module deliberately does NOT apply event payloads to rows. It resolves each event to
//! the container(s) whose listing would show the change, and hands those to the existing
//! [`refresh_container`](crate::auth::AuthCacheState::refresh_container). Three hard problems
//! disappear with that choice, and none of them can come back:
//!
//! - **Order stops mattering.** A container is re-listed once no matter how many events named
//!   it, so the log's `id` ordering (which does not always agree with its own timestamps) and
//!   any within-second interleaving are irrelevant.
//! - **Terminality stops mattering.** Whether a `fileVersioned` without a successor retired the
//!   lineage for good, or a `fileTrash` merely trashed it, is the listing's answer to give —
//!   we never guess, and never delete a row on an inference.
//! - **Thin payloads stop mattering.** The log omits `color`/`favorited` on folder events and
//!   the full record on `itemFavorite`; reconstructing rows from that would silently clobber
//!   columns the event never carried.
//!
//! The listing remains the authority, exactly as it is for a browse. Replay only decides where
//! to point it.
//!
//! ## What forces the full pass anyway
//!
//! Replay is an optimization and refuses rather than guesses. [`ReplayOutcome::Fallback`] is
//! returned when there is no cursor yet, when an event in the range cannot be decoded or
//! attributed — a file event without the lineage id the server has logged since 2026-09-01
//! included — when `deleteAll` appears, when paging cannot reach the cursor, or when the
//! targeted set grows big enough that the sweep is cheaper anyway.
//!
//! ## Paging without a sub-second cursor
//!
//! `v3/user/events` takes one whole-seconds `timestamp` and returns the newest page of events
//! strictly BEFORE that second, so paging backwards means re-asking with one second past the
//! oldest second seen — the boundary second is fetched again in full, because a page may have
//! split it. A second holding more events than one page therefore cannot be paged through:
//! asking again returns the same page. That is detected rather than assumed — a page
//! contributing no new event id is no progress, and no progress falls back. The cursor is a
//! whole SECOND and the range is re-applied inclusively, which needs no id bookkeeping because
//! re-listing a container is idempotent.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use filen_sdk_rs::user::events::{DecryptedUserEvent, DecryptedUserEventKind};
use filen_types::fs::{ObjectType, ParentUuid, Uuid};
use rusqlite::Connection;

use crate::sql::item::RawDBItem;

/// Pages to walk before a full pass is simply cheaper. At ~100 events per page this covers a
/// gap of a few thousand events; past that the sweep wins on round trips alone.
const MAX_REPLAY_PAGES: usize = 20;

/// Containers to re-list before a full pass is cheaper. The account in the incident that
/// prompted this holds 2,976, so anything near this bound is a bulk operation the sweep
/// handles in one shot.
const MAX_TARGETED_CONTAINERS: usize = 64;

/// Clock-skew slack on the first request, mirroring the SDK's own default cutoff.
const SKEW: chrono::TimeDelta = chrono::TimeDelta::seconds(60);

/// What the caller must do to close the gap.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReplayOutcome {
	/// Every event was attributed. Re-list these containers (and the trash, if asked) and the
	/// cache is caught up; `cursor` is the second to resume from next time.
	Targeted {
		containers: HashSet<Uuid>,
		relist_trash: bool,
		cursor: DateTime<Utc>,
	},
	/// Replay cannot close this gap — run the full pass.
	Fallback,
}

/// Where one event's effect would show up.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EventTarget {
	/// Refresh these listings; each is authoritative for whatever the event did to what it
	/// holds. `trash` is a listing like any other here — a trashed row is not in ANY directory
	/// listing, so for an item this cache holds trashed, the trash listing is the only place its
	/// change can show up.
	Refresh { containers: Vec<Uuid>, trash: bool },
	/// Names nothing this cache holds. The live path's relevance gate drops the same events:
	/// a container this device never listed is listed on first browse anyway.
	Irrelevant,
	/// Cannot be attributed; only a full pass closes it.
	Unknown,
}

/// The account-level and sharing kinds, listed rather than defaulted.
///
/// None of them touch a row in this cache — it mirrors the owner's own drive tree, and the
/// live socket subscription ([`crate::live`]'s event list) omits every one of them for the
/// same reason. Skipping them explicitly is what keeps ordinary login/share/link traffic from
/// forcing a full pass through the [`EventTarget::Unknown`] arm.
fn is_out_of_scope(kind: &DecryptedUserEventKind) -> bool {
	use DecryptedUserEventKind as K;
	matches!(
		kind,
		K::Login(_)
			| K::FailedLogin(_)
			| K::PasswordChanged(_)
			| K::TwoFaEnabled(_)
			| K::TwoFaDisabled(_)
			| K::RequestAccountDeletion(_)
			| K::DeleteUnfinished(_)
			| K::CodeRedeemed(_)
			| K::EmailChanged(_)
			| K::EmailChangeAttempt(_)
			| K::RemovedSharedInItems(_)
			| K::RemovedSharedOutItems(_)
			| K::FileShared(_)
			| K::FolderShared(_)
			| K::FileLinkEdited(_)
			| K::FolderLinkEdited(_)
			// Only archived versions died; this cache holds heads only.
			| K::DeleteVersioned(_)
	)
}

/// Resolves one event to the listings that would show its effect.
///
/// Two lookups, unioned, and that union is the whole trick: the listing named by the event AND
/// the listing our row currently sits in. A move needs both — the destination to gain the item,
/// the origin to lose it — and every other kind degenerates to one of them. An event naming
/// neither is for a part of the drive this device has never listed.
///
/// "The listing our row sits in" includes the TRASH. Half the kinds carry no parent at all
/// (`itemFavorite`, the rename and metadata kinds), so the row is the only thing that can place
/// them, and a row this cache holds trashed is in no directory listing — reading only its
/// directory parent drops those events on the floor and leaves the trash view stale.
pub(crate) fn target_for(conn: &Connection, event: &DecryptedUserEvent) -> EventTarget {
	use DecryptedUserEventKind as K;
	if is_out_of_scope(&event.kind) {
		return EventTarget::Irrelevant;
	}
	let (uuid, stable, parent) = match &event.kind {
		// Everything on the drive is gone — or so the event says. Never replayed: the full
		// pass tells "gone" from "unreachable" per container, and its guarded forgets keep any
		// unsent local edit.
		K::DeleteAll(_) => return EventTarget::Unknown,
		// The trash relist owns its rows, and it reconciles them properly (per-row probes,
		// pending markers released on a definitive not-found).
		K::TrashEmptied(_) => {
			return EventTarget::Refresh {
				containers: Vec::new(),
				trash: true,
			};
		}
		// Every file kind here has carried its lineage id since the server began logging it, on
		// 2026-09-01. An event without one is older than that, or a server anomaly, and its uuid
		// alone cannot tell a lineage this cache holds under a re-minted uuid from a part of the
		// drive it never listed — resolving it anyway drops it as irrelevant and closes the gap
		// over a stale container. Refused instead, like every other event replay cannot read.
		K::FileUploaded(i)
		| K::FileVersioned(i)
		| K::FileRestored(i)
		| K::VersionedFileRestored(i)
		| K::FileMoved(i)
		| K::FileRm(i) => {
			let Some(stable) = i.stable_uuid else {
				return EventTarget::Unknown;
			};
			(i.uuid, Some(Uuid::from(stable)), i.parent)
		}
		// The one kind whose missing stable id is a message rather than an age: only archived
		// versions died, the live head stands, and the uuid it names resolves as usual.
		K::DeleteFilePermanently(i) => (i.uuid, i.stable_uuid.map(Uuid::from), i.parent),
		// With `newUUID` set this is a versioning-disabled edit's retirement of the old uuid, and
		// its stable id is the retired row's freshly minted one — never the lineage's, which lives
		// on under the successor — so it can name no row we hold; the uuid does.
		K::FileTrash(i) => {
			let Some(stable) = i.stable_uuid else {
				return EventTarget::Unknown;
			};
			(
				i.uuid,
				i.new_uuid.is_none().then(|| Uuid::from(stable)),
				i.parent,
			)
		}
		K::FileRenamed(i) | K::FileMetadataChanged(i) => {
			let Some(stable) = i.stable_uuid else {
				return EventTarget::Unknown;
			};
			(i.uuid, Some(Uuid::from(stable)), None)
		}
		// Folder favourites never carry a stable id; file ones always have.
		K::ItemFavorite(i) => {
			if i.item_type == Some(ObjectType::File) && i.stable_uuid.is_none() {
				return EventTarget::Unknown;
			}
			(i.uuid, i.stable_uuid.map(Uuid::from), None)
		}
		// `baseFolderCreated` is deliberately NOT here: its payload has never been observed
		// live (see the wire type's own doc), and classifying an unverified shape as
		// attributable would drop it silently if it turns out to carry no parent. It fires
		// about once in an account's life, so the pass is free.
		K::BaseFolderCreated(_) => return EventTarget::Unknown,
		K::FolderTrash(i)
		| K::FolderMoved(i)
		| K::SubFolderCreated(i)
		| K::FolderRestored(i)
		| K::DeleteFolderPermanently(i) => (i.uuid, None, i.parent),
		K::FolderRenamed(i) | K::FolderMetadataChanged(i) => (i.uuid, None, None),
		K::FolderColorChanged(i) => (i.uuid, None, None),
		// Reached only if a new kind is added above without a decision here; a full pass is
		// the safe reading of "we do not know what this did".
		_ => return EventTarget::Unknown,
	};

	let mut containers = Vec::new();
	let mut trash = false;
	// The container the event names, if this device holds it.
	if let Some(parent) = parent
		&& holds(conn, parent)
	{
		containers.push(parent);
	}
	// Where our row sits today. Resolved by the whole-life id first: an edit re-mints a file's
	// uuid, so a rename that arrived after one names a uuid we no longer hold, while the stable
	// id still finds the row.
	if let Some(row) = row_for(conn, uuid, stable) {
		match row.parent {
			Some(ParentUuid::Uuid(current)) => {
				if !containers.contains(&current) {
					containers.push(current);
				}
			}
			// Trashed here: the trash listing is this row's container, and `update_trash`
			// reconciles it the same way a directory listing reconciles its own.
			Some(ParentUuid::Trash(_)) => trash = true,
			// A row parented to a virtual container, or the account root itself, has no single
			// listing of its own to refresh. Rare enough to hand to the pass rather than guess.
			Some(ParentUuid::Recents | ParentUuid::Favorites | ParentUuid::Links) | None => {
				return EventTarget::Unknown;
			}
		}
	}

	if containers.is_empty() && !trash {
		// Nothing held under either id. If the event carried no identity at all we cannot tell
		// "not ours" from "unreadable", so only an identified event may be dropped as
		// irrelevant — an anonymous one forces the pass.
		if uuid.is_none() && stable.is_none() {
			return EventTarget::Unknown;
		}
		return EventTarget::Irrelevant;
	}
	EventTarget::Refresh { containers, trash }
}

fn holds(conn: &Connection, uuid: Uuid) -> bool {
	matches!(RawDBItem::select(conn, uuid), Ok(Some(_)))
}

fn row_for(conn: &Connection, uuid: Option<Uuid>, stable: Option<Uuid>) -> Option<RawDBItem> {
	if let Some(stable) = stable
		&& let Ok(Some(row)) = RawDBItem::select_by_stable(conn, stable)
	{
		return Some(row);
	}
	RawDBItem::select(conn, uuid?).unwrap_or_default()
}

/// Folds a page of events into the running target set, or gives up on the first thing replay
/// cannot attribute.
///
/// Only events at or after `since` are folded. A page reaches back about a hundred events
/// whatever the gap actually was, and those older ones are by definition already accounted for:
/// folding them re-lists containers that are current, and — the part that bites — lets an event
/// the cache settled with long ago refuse the gap. One `deleteAll` or unreadable payload sitting
/// in the recent feed would otherwise force a full sweep on every reconnect for as long as it
/// stayed on the first page.
pub(crate) fn fold_targets(
	conn: &Connection,
	events: &[DecryptedUserEvent],
	since: DateTime<Utc>,
	containers: &mut HashSet<Uuid>,
	relist_trash: &mut bool,
) -> bool {
	for event in events.iter().filter(|event| event.timestamp >= since) {
		match target_for(conn, event) {
			EventTarget::Refresh {
				containers: found,
				trash,
			} => {
				containers.extend(found);
				*relist_trash |= trash;
			}
			EventTarget::Irrelevant => {}
			EventTarget::Unknown => {
				tracing::info!(
					"event {} ({}) cannot be replayed; falling back to a full pass",
					event.id,
					event.kind.event_type()
				);
				return false;
			}
		}
		if containers.len() > MAX_TARGETED_CONTAINERS {
			tracing::info!(
				"replay reached {} containers; a full pass is cheaper",
				containers.len()
			);
			return false;
		}
	}
	true
}

/// Whether this gap may be closed from the event log at all.
///
/// Two refusals, and the first is the load-bearing one. A recorded DEBT (`pending_reconcile`) is
/// the full pass's to answer and never a replay's: it is set for containers no event will ever
/// name — one that failed to converge, one reported materialized for the first time whose
/// contents may be months old — so a replay reporting the gap closed would leave exactly the
/// staleness the debt exists to fix and clear nothing, while the sweep it owes never runs.
/// Second, with no cursor there is no lower bound to page back to.
pub(crate) fn replay_allowed(owed: bool, cursor: Option<DateTime<Utc>>) -> bool {
	!owed && cursor.is_some()
}

/// Whether a page moved the walk forward. A page that adds no id we have not already seen
/// means the cutoff cannot go lower — the boundary second holds more events than one page —
/// and no amount of re-asking will produce the rest.
pub(crate) fn made_progress(seen: &mut HashSet<u64>, page: &[DecryptedUserEvent]) -> bool {
	let mut fresh = false;
	for event in page {
		fresh |= seen.insert(event.id);
	}
	fresh
}

/// The cutoff for the next page: one second past the oldest second this page reached. The feed
/// answers strictly BEFORE the cutoff, and a page may have split its oldest second, so the next
/// page has to ask for that second again in full — the events already seen come back with it,
/// which is free, and a second holding more than a page repeats and is caught by
/// [`made_progress`]. Returns `None` for an empty page (the feed is exhausted).
pub(crate) fn next_cutoff(page: &[DecryptedUserEvent]) -> Option<DateTime<Utc>> {
	page.iter()
		.map(|event| event.timestamp)
		.min()
		.map(|oldest| oldest + chrono::TimeDelta::seconds(1))
}

/// The newest second the walk covered — the cursor to resume from. Inclusive on purpose: the
/// newest second may still be gaining events, and re-listing a container twice is free.
pub(crate) fn newest(events: &[DecryptedUserEvent]) -> Option<DateTime<Utc>> {
	events.iter().map(|event| event.timestamp).max()
}

/// The first cutoff to ask for: now, plus slack for a clock the server does not share.
pub(crate) fn first_cutoff(now: DateTime<Utc>) -> DateTime<Utc> {
	now + SKEW
}

/// The cursor a full pass may claim, from the local clock reading `now`.
///
/// A pass covers everything up to the moment it started, so the obvious stamp is that moment —
/// but the value is compared against SERVER event timestamps ([`fold_targets`]), and the two
/// clocks are not the same clock. Stamp it straight and a device running a minute fast writes a
/// cursor a minute ahead of the server: every event the server timestamps inside that window is
/// dropped by the filter, silently, and because replay then reports the gap closed, the cursor
/// and the watermark both advance past containers nothing re-listed. That is undetectable and
/// permanent, which is the one failure this whole path exists to remove.
///
/// So it leans back by the same slack [`first_cutoff`] leans forward. Both errors then cost the
/// same harmless thing — re-folding events already accounted for, which re-lists a container
/// that was already current.
pub(crate) fn pass_cursor(now: DateTime<Utc>) -> DateTime<Utc> {
	now - SKEW
}

/// Whether the walk has reached the cursor and can stop. `cutoff` is the NEXT page's cutoff
/// ([`next_cutoff`], one past the oldest second seen), so this holds once a page reached a
/// second strictly older than the cursor's — which is the only proof that the cursor's own
/// second came back whole rather than split across a page boundary.
pub(crate) fn reached(cutoff: DateTime<Utc>, cursor: DateTime<Utc>) -> bool {
	cutoff <= cursor
}

/// Page budget exhausted — a gap this long is cheaper to close with one pass.
pub(crate) fn page_budget_exhausted(pages: usize) -> bool {
	pages >= MAX_REPLAY_PAGES
}

#[cfg(test)]
mod tests;
