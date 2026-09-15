//! Grouped sync-engine test suite — ONE integration-test binary, one module per category.
//!
//! Run all:          cargo test -p filen-sdk-rs -F sync-engine --test sync_suite
//! Run one category: cargo test -p filen-sdk-rs -F sync-engine --test sync_suite -- basics::
//!
//! Shared scaffolding lives in `harness`.

mod helpers;

#[path = "sync_suite/harness.rs"]
mod harness;

#[path = "sync_suite/basics.rs"]
mod basics;
#[path = "sync_suite/conflicts.rs"]
mod conflicts;
#[path = "sync_suite/control.rs"]
mod control;
#[path = "sync_suite/convergence.rs"]
mod convergence;
#[path = "sync_suite/deletions.rs"]
mod deletions;
#[path = "sync_suite/filters.rs"]
mod filters;
#[path = "sync_suite/matrix.rs"]
mod matrix;
#[path = "sync_suite/migration.rs"]
mod migration;
#[path = "sync_suite/modes.rs"]
mod modes;
#[path = "sync_suite/moves.rs"]
mod moves;
#[path = "sync_suite/observability.rs"]
mod observability;
#[path = "sync_suite/paths.rs"]
mod paths;
#[path = "sync_suite/races.rs"]
mod races;
#[path = "sync_suite/resilience.rs"]
mod resilience;
#[path = "sync_suite/scale.rs"]
mod scale;
#[path = "sync_suite/security.rs"]
mod security;
#[path = "sync_suite/watch.rs"]
mod watch;
