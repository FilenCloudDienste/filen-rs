//! Copying drive items: there is no server-side copy (items are end-to-end encrypted), so a
//! copy reads each decrypted source and writes a new encrypted item.

// The copy engine that consumes these arrives in later commits; until then only tests do.
#[allow(dead_code)]
pub(crate) mod naming;
#[allow(dead_code)]
pub(crate) mod plan;
