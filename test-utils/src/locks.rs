//! Names of the resource locks tests take on the shared test accounts. Tests in different
//! binaries (and CI legs, and the wasm tests in `filen-sdk-rs/web/main.test.ts`) serialize on
//! account-wide state only by taking the same name, so each lives here once.

/// A file's version history: held by tests that upload over a same-name file and read the
/// versions it leaves, and by every test that changes or pins the versioning setting (through
/// [`crate::with_versioning`]); always taken before [`USER_VERSIONING`].
pub const VERSIONS: &str = "test:versions";

/// The account-wide file-versioning setting. What a same-name upload does depends on it: with it
/// on the old file becomes a version of the new one, with it off the old file goes to the trash.
/// Two rules keep the tests that depend on it apart:
///
/// - a test that changes the setting holds [`VERSIONS`] and then this lock for as long as the
///   setting is changed, and puts it back even when it panics;
/// - a test that relies on the setting's value holds at least one of the two, and pins the value
///   it needs instead of assuming the account's.
///
/// So a test holding either lock never runs while another test has the setting changed.
/// [`crate::with_versioning`] does both.
pub const USER_VERSIONING: &str = "test:user-versioning";

/// The trash: emptying it is account-wide, so a test that trashes something and then looks for
/// it there holds this across both.
pub const TRASH: &str = "test:rs:trash";

/// The account's notes, held by tests that create notes or list them.
pub const NOTES: &str = "test:notes";

/// The account's chats, and the avatar and nickname chat messages carry.
pub const CHATS: &str = "test:chats";

/// The contacts between the test account and the share account (taken on both clients).
pub const CONTACT: &str = "test:contact";
