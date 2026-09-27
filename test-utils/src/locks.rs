//! Names of the resource locks tests take on the shared test accounts. Tests in different
//! binaries (and CI legs, and the wasm tests in `filen-sdk-rs/web/main.test.ts`) serialize on
//! account-wide state only by taking the same name, so each lives here once.

/// A file's version history: held by tests that upload over a same-name file and read the
/// versions it leaves, taken before [`USER_VERSIONING`] when a test holds both.
pub const VERSIONS: &str = "test:versions";

/// The account-wide file-versioning setting, held by tests that change or rely on it.
pub const USER_VERSIONING: &str = "test:user-versioning";

/// The trash: emptying it is account-wide, so a test that trashes something and then looks for
/// it there holds this across both.
pub const TRASH: &str = "test:rs:trash";

/// The account's chats, and the avatar and nickname chat messages carry.
pub const CHATS: &str = "test:chats";

/// The contacts between the test account and the share account (taken on both clients).
pub const CONTACT: &str = "test:contact";
