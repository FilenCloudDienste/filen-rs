//! Copying drive items: there is no server-side copy (items are end-to-end encrypted), so a
//! copy reads each decrypted source and writes a new encrypted item.

// The public copy API and its bindings arrive in later commits; until then only tests reach
// the engine.
#[allow(dead_code)]
pub(crate) mod backend;
#[allow(dead_code)]
pub(crate) mod control;
#[allow(dead_code)]
pub(crate) mod engine;
#[allow(dead_code)]
pub(crate) mod naming;
#[allow(dead_code)]
pub(crate) mod plan;
#[allow(dead_code)]
pub(crate) mod progress;
#[allow(dead_code)]
pub(crate) mod report;
