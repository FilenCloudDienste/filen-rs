//! The helper's stdio protocol, version [`PROTOCOL_VERSION`]: one UTF-8 JSON object per line,
//! requests on stdin and replies and events on stdout. A request names itself in `t` and carries
//! an `id` and its fields beside them (`{"t":"register","id":1,"root":..}`); it is not the desktop
//! app's native `{"t":"call","id","op","args"}` envelope. A reply is
//! `{"t":"reply","id","ok":true,"value"}` or `{"t":"reply","id","ok":false,"error":{"code","message"}}`,
//! as the native protocol's replies are, with `message` at most [`MAX_MESSAGE_CHARS`] characters;
//! every other line is an event, named by its `t`.
//!
//! Every message is checked against its schema here, OS-neutrally, so the golden fixtures in
//! `tests/fixtures/protocol/v1.jsonl` hold on every host: unknown and duplicate fields are refused,
//! as are fields out of range. What depends on the OS (whether a path is absolute, on NTFS, a
//! drive root) is checked by the command loop. No error message quotes a pushed value, as a
//! pushed session's secrets travel in the same lines.

use std::{
	fmt,
	ops::Deref,
	path::{Path, PathBuf},
};

use filen_types::fs::Uuid;
use serde::{Deserialize, Serialize, Serializer, de::IgnoredAny, ser::SerializeStruct};
use serde_json::{Map, Value};

use crate::{
	auth::{AuthPayload, AuthPayloadError, KeyAccount},
	failure::Failure,
	local::ConflictKind,
};

/// The protocol's version, sent in `hello`. Both sides require an exact match, and every change to
/// a message bumps it, an added field included, as both sides refuse unknown fields.
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest input line taken, in bytes, its newline excluded: a pushed session of 1 MiB
/// escaped as a JSON string fits with room to spare.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The most entries one `activity` or `skippedNames` line, or one `unregister` reply's `kept`
/// list, carries.
pub const MAX_LIST_ENTRIES: usize = 1000;

/// The most characters an error's `message` holds, as the desktop's native protocol takes; a
/// longer one is cut, ending in `…`.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// The largest request id: the largest integer a JavaScript number holds exactly.
const MAX_REQUEST_ID: u64 = (1 << 53) - 1;

/// The most UTF-16 units a sync root path holds, as Windows's long paths do.
const MAX_ROOT_UNITS: usize = 32_767;

/// The most digits a user id holds: `u64::MAX` has 20.
const MAX_USER_ID_DIGITS: usize = 20;

/// The id the parent gave a request, which its reply carries: 0 to 2^53 - 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
	/// `id` as a request id, if it is at most 2^53 - 1.
	pub fn new(id: u64) -> Option<Self> {
		(id <= MAX_REQUEST_ID).then_some(Self(id))
	}

	/// The id.
	pub fn get(self) -> u64 {
		self.0
	}
}

/// A Filen user id as the parent spells it: 1 to 20 decimal digits without a leading zero, at
/// most `u64::MAX`. On the wire it is a string, as JavaScript holds no `u64` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct UserId(u64);

impl UserId {
	/// `text` as a user id, if it follows the rule on [`UserId`].
	pub fn parse(text: &str) -> Option<Self> {
		let digits = text.len() <= MAX_USER_ID_DIGITS
			&& !text.starts_with('0')
			&& !text.is_empty()
			&& text.bytes().all(|b| b.is_ascii_digit());
		if !digits {
			return None;
		}
		text.parse().ok().map(Self)
	}

	/// The id as a number.
	pub fn get(self) -> u64 {
		self.0
	}

	/// The key account the user's sealing key is stored under: the id's digits.
	pub fn key_account(self) -> KeyAccount {
		KeyAccount::new(&self.0.to_string())
			.expect("decimal digits are a key account (should be impossible)")
	}
}

impl From<u64> for UserId {
	fn from(id: u64) -> Self {
		Self(id)
	}
}

impl fmt::Display for UserId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(&self.0, f)
	}
}

impl From<UserId> for String {
	fn from(user: UserId) -> Self {
		user.to_string()
	}
}

/// A sync root's path as the protocol carries it: 1 to 32,767 UTF-16 units, as Windows's long
/// paths hold, with no NUL; UTF-8, as it came in as a JSON string. Whether it is a valid root on
/// this OS is the command loop's check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RootDir(String);

/// Why a path cannot be a [`RootDir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("root is not 1 to 32,767 UTF-16 units without a NUL")]
pub struct RootDirError;

impl RootDir {
	/// `path` as a root directory, if it follows the rule on [`RootDir`].
	pub fn parse(path: String) -> Result<Self, RootDirError> {
		let units = path.encode_utf16().count();
		if units == 0 || units > MAX_ROOT_UNITS || path.contains('\0') {
			return Err(RootDirError);
		}
		Ok(Self(path))
	}

	/// The path.
	pub fn as_path(&self) -> &Path {
		Path::new(&self.0)
	}

	/// The path as the wire spells it.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl Deref for RootDir {
	type Target = Path;

	fn deref(&self) -> &Path {
		self.as_path()
	}
}

impl AsRef<Path> for RootDir {
	fn as_ref(&self) -> &Path {
		self.as_path()
	}
}

/// A `register` request: the root to serve for a user, and where the user's data lives.
#[derive(Debug)]
pub struct Registration {
	/// The sync root.
	pub root: RootDir,
	/// Whose root it is.
	pub user: UserId,
	/// The user's data directory, as given: the command loop checks it is absolute and UTF-8, and
	/// missing, empty or one the helper created (`dataDirNotEmpty` otherwise).
	pub data_dir: PathBuf,
}

/// What the parent asks.
#[derive(Debug)]
pub enum Request {
	/// Registers a root for a user, or resumes serving it when a sealed session is on disk.
	Register(Registration),
	/// Seals a session for the registered user and serves the root with it. Answered once the cache
	/// signed in, within [`HelperConfig::auth_wait`](crate::HelperConfig::auth_wait) (30 s), or
	/// sooner with `shuttingDown` when a `shutdown` arrives or the input closes meanwhile.
	SetAuth {
		/// Whose session it is.
		user: UserId,
		/// The checked `AuthFile`, enabled and carrying a session; boxed, as it is far larger than
		/// any other request.
		payload: Box<AuthPayload>,
	},
	/// Stops serving a user's root and deletes their session, what the cache kept and the sealing
	/// key.
	ClearAuth {
		/// Whose session to clear.
		user: UserId,
	},
	/// Unregisters a user's root without losing local changes, and deletes their data, session and
	/// sealing key. On sign-out the parent sends it while the root is still served, before any
	/// `clearAuth`: only a served root fetches the data of a changed file not all on disk first;
	/// without a session such a file stops the unregister (`unsyncedChanges`) and the root stays
	/// registered. It has no time bound, as it walks the whole root and may download, and requests
	/// sent after it wait for it.
	Unregister {
		/// Whose root to unregister.
		user: UserId,
		/// Whether clean downloaded files are deleted too, rather than left as ordinary files.
		delete_hydrated: bool,
	},
	/// Lists the roots the helper knows.
	ListRoots,
	/// The helper's state. While a root is served, its pending count waits for a walk of the whole
	/// root, at most a few seconds, and holds up the requests behind it meanwhile; it is null past
	/// that, or once a stop was asked for.
	GetState,
	/// Stops serving and exits.
	Shutdown,
}

/// A request and the id its reply carries.
#[derive(Debug)]
pub struct InMessage {
	/// The request's id.
	pub id: RequestId,
	/// The request.
	pub request: Request,
}

/// The codes a reply's error, a `protocolError` or the command loop's refusals carry, spelled on
/// the wire as their names in camelCase. The desktop app's `DesktopFileProviderErrorCode` has no
/// counterpart for most of them yet: its Windows adapter maps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
	/// The line is not JSON.
	InvalidJson,
	/// The line is JSON but not a message as its schema says.
	InvalidMessage,
	/// The message's `t` names no request.
	UnknownMessage,
	/// The line is longer than [`MAX_LINE_BYTES`].
	LineTooLong,
	/// The line is not UTF-8.
	InvalidUtf8,
	/// The data directory is not absolute or not UTF-8.
	DataDirInvalid,
	/// The data directory holds something and is not one the helper created: nothing is kept in
	/// it or deleted from it.
	DataDirNotEmpty,
	/// The helper is stopping.
	ShuttingDown,
	/// A file operation failed.
	Io,
	/// The OS refused an operation.
	Os,
	/// A broken invariant in the helper.
	Internal,
	/// The request needs a registered root.
	NotRegistered,
	/// Another root or user is registered.
	AlreadyRegistered,
	/// The root is another user's, or another provider's.
	RootInUse,
	/// The root is not a path a sync root can be.
	RootInvalid,
	/// The root is not on an NTFS volume.
	NotNtfs,
	/// The root overlaps the data directory, the state directory or another root.
	RootOverlap,
	/// The root is a directory that holds something and is not this provider's root.
	RootNotEmpty,
	/// The session is not one the provider can serve.
	PayloadInvalid,
	/// The session belongs to another user or account.
	OwnerMismatch,
	/// The cache did not sign in with the session.
	SessionRejected,
	/// The key store failed.
	CredentialStore,
	/// The root holds local changes that cannot be kept safely.
	UnsyncedChanges,
}

/// Why the helper refused to start: sent as the only line before it exits, with the exit code
/// [`FatalCode::exit_code`]. Spelled on the wire as its name in camelCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FatalCode {
	/// The command line is wrong.
	Usage,
	/// The helper runs elevated, which Credential Manager and the user's sync roots refuse.
	Elevated,
	/// Windows is older than the provider supports.
	Unsupported,
	/// Another helper holds the state directory.
	AlreadyRunning,
	/// The state directory is not usable.
	StateDirInvalid,
	/// The OS is not Windows.
	NotWindows,
	/// The helper's async runtime could not be built.
	RuntimeFailed,
}

impl FatalCode {
	/// The process's exit code for this refusal.
	pub fn exit_code(self) -> u8 {
		match self {
			Self::Usage => 2,
			Self::Elevated => 10,
			Self::Unsupported => 11,
			Self::AlreadyRunning => 12,
			Self::StateDirInvalid => 13,
			Self::NotWindows => 14,
			Self::RuntimeFailed => 15,
		}
	}
}

/// Why a line is not a request; answered with a reply when `id` parsed, and with a
/// `protocolError` otherwise. `message` is English, for logs, and quotes nothing of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
	/// The line's id, when it parsed.
	pub id: Option<RequestId>,
	/// Why.
	pub code: ErrorCode,
	/// What was wrong, for logs.
	pub message: String,
}

impl ParseError {
	fn new(id: Option<RequestId>, code: ErrorCode, message: impl Into<String>) -> Self {
		Self {
			id,
			code,
			message: message.into(),
		}
	}
}

/// What serde_json found wrong, by kind and position only: its message can quote the line.
fn schema_error(id: Option<RequestId>, error: &serde_json::Error) -> ParseError {
	ParseError::new(
		id,
		ErrorCode::InvalidMessage,
		format!(
			"the message does not match its schema ({:?} at column {})",
			error.classify(),
			error.column()
		),
	)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RegisterWire {
	#[serde(rename = "t")]
	_t: IgnoredAny,
	#[serde(rename = "id")]
	_id: IgnoredAny,
	root: String,
	user_id: String,
	data_dir: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetAuthWire {
	#[serde(rename = "t")]
	_t: IgnoredAny,
	#[serde(rename = "id")]
	_id: IgnoredAny,
	user_id: String,
	json: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UserWire {
	#[serde(rename = "t")]
	_t: IgnoredAny,
	#[serde(rename = "id")]
	_id: IgnoredAny,
	user_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UnregisterWire {
	#[serde(rename = "t")]
	_t: IgnoredAny,
	#[serde(rename = "id")]
	_id: IgnoredAny,
	user_id: String,
	delete_hydrated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BareWire {
	#[serde(rename = "t")]
	_t: IgnoredAny,
	#[serde(rename = "id")]
	_id: IgnoredAny,
}

/// `text` as a user id, or why not.
fn user_id(id: RequestId, text: &str) -> Result<UserId, ParseError> {
	UserId::parse(text).ok_or_else(|| {
		ParseError::new(
			Some(id),
			ErrorCode::InvalidMessage,
			"userId is not 1 to 20 decimal digits without a leading zero, at most 2^64 - 1",
		)
	})
}

/// Parses one input line, its newline already stripped.
pub fn parse_line(line: &str) -> Result<InMessage, ParseError> {
	// Read first as an object, for the reply's id and the type; a struct would take an array too.
	// Each request's own schema below refuses unknown and duplicate fields.
	let mut head: Map<String, Value> =
		serde_json::from_str(line).map_err(|e| match e.classify() {
			serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
				ParseError::new(None, ErrorCode::InvalidJson, "the line is not JSON")
			}
			serde_json::error::Category::Data | serde_json::error::Category::Io => {
				schema_error(None, &e)
			}
		})?;
	let id = match head.remove("id") {
		Some(value) => Some(value.as_u64().and_then(RequestId::new).ok_or_else(|| {
			ParseError::new(
				None,
				ErrorCode::InvalidMessage,
				"id is not an integer from 0 to 2^53 - 1",
			)
		})?),
		None => None,
	};
	let Some(Value::String(t)) = head.remove("t") else {
		return Err(ParseError::new(
			id,
			ErrorCode::InvalidMessage,
			"the message has no string t",
		));
	};
	let Some(id) = id else {
		return Err(ParseError::new(
			None,
			ErrorCode::InvalidMessage,
			"the message has no id",
		));
	};
	let schema = |e: serde_json::Error| schema_error(Some(id), &e);
	let request = match t.as_str() {
		"register" => {
			let wire: RegisterWire = serde_json::from_str(line).map_err(schema)?;
			Request::Register(registration(id, wire)?)
		}
		"setAuth" => {
			let wire: SetAuthWire = serde_json::from_str(line).map_err(schema)?;
			let user = user_id(id, &wire.user_id)?;
			Request::SetAuth {
				user,
				payload: Box::new(session_payload(id, &wire.json)?),
			}
		}
		"clearAuth" => {
			let wire: UserWire = serde_json::from_str(line).map_err(schema)?;
			Request::ClearAuth {
				user: user_id(id, &wire.user_id)?,
			}
		}
		"unregister" => {
			let wire: UnregisterWire = serde_json::from_str(line).map_err(schema)?;
			Request::Unregister {
				user: user_id(id, &wire.user_id)?,
				delete_hydrated: wire.delete_hydrated,
			}
		}
		"listRoots" | "getState" | "shutdown" => {
			let _: BareWire = serde_json::from_str(line).map_err(schema)?;
			match t.as_str() {
				"listRoots" => Request::ListRoots,
				"getState" => Request::GetState,
				_ => Request::Shutdown,
			}
		}
		_ => {
			return Err(ParseError::new(
				Some(id),
				ErrorCode::UnknownMessage,
				"t names no request of this protocol version",
			));
		}
	};
	Ok(InMessage { id, request })
}

/// A `register`'s fields, checked.
fn registration(id: RequestId, wire: RegisterWire) -> Result<Registration, ParseError> {
	let root = RootDir::parse(wire.root)
		.map_err(|e| ParseError::new(Some(id), ErrorCode::RootInvalid, e.to_string()))?;
	if wire.data_dir.is_empty() || wire.data_dir.contains('\0') {
		return Err(ParseError::new(
			Some(id),
			ErrorCode::DataDirInvalid,
			"dataDir is empty or holds a NUL",
		));
	}
	let user = user_id(id, &wire.user_id)?;
	Ok(Registration {
		root,
		user,
		data_dir: PathBuf::from(wire.data_dir),
	})
}

/// A `setAuth`'s session, checked: an `AuthFile` as the cache parses it, the provider enabled.
fn session_payload(id: RequestId, json: &str) -> Result<AuthPayload, ParseError> {
	let invalid = |message: String| ParseError::new(Some(id), ErrorCode::PayloadInvalid, message);
	let payload = AuthPayload::parse(json.as_bytes())
		.map_err(|e: AuthPayloadError| invalid(e.to_string()))?;
	if !payload.provider_enabled() {
		return Err(invalid("the session has the provider disabled".to_owned()));
	}
	Ok(payload)
}

/// The helper's state, as `status` and replies name it, spelled in camelCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PhaseName {
	/// No root registered.
	Idle,
	/// A root registered, not served.
	Registered,
	/// A root served.
	Serving,
	/// Stopping.
	Stopping,
}

/// Why `unregister` kept a local item, spelled in camelCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeptItemReason {
	/// A placeholder written to since it was last in sync.
	Modified,
	/// An ordinary item the drive never had.
	NotUploaded,
	/// The local side of a conflict, never uploaded.
	ConflictCopy,
	/// A placeholder whose identity this build cannot read.
	UnknownPlaceholder,
	/// A link or reparse point, never followed.
	Unsupported,
	/// A clean downloaded file another process held open, left as an ordinary file.
	InUse,
	/// An item in this directory whose name the provider cannot spell; the directory is kept as
	/// an ordinary one with it.
	UnrepresentableName,
}

/// A local item `unregister` kept, by its path below the root as Windows spells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeptItem {
	/// Its path below the root.
	pub path: String,
	/// Why it was kept.
	pub reason: KeptItemReason,
}

/// Names what one unregister kept, the same each time it is reported: 16 lowercase hex digits on
/// the wire. A report the parent may not have read (the helper was killed before the parent's next
/// request) is reported again, by a later `register` or `unregister`, so the parent tells the user
/// once per id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct KeptId(u64);

impl KeptId {
	/// A new id, random.
	pub(crate) fn random() -> Self {
		Self(rand::random())
	}

	/// `text` as an id, if it is 16 lowercase hex digits.
	pub(crate) fn parse(text: &str) -> Option<Self> {
		let digits =
			text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
		digits
			.then(|| u64::from_str_radix(text, 16).ok())
			.flatten()
			.map(Self)
	}
}

impl From<u64> for KeptId {
	fn from(id: u64) -> Self {
		Self(id)
	}
}

impl fmt::Display for KeptId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:016x}", self.0)
	}
}

impl From<KeptId> for String {
	fn from(id: KeptId) -> Self {
		id.to_string()
	}
}

/// What an earlier unregister of the user's root kept, which no reply the parent answered has
/// reported yet: the helper was killed before it could, or before the parent's next request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptEarlier {
	/// Names this report.
	pub id: KeptId,
	/// The first [`MAX_LIST_ENTRIES`] items kept; empty when that unregister found the root let go
	/// already and could only count them.
	pub kept: Vec<KeptItem>,
	/// How many items were kept.
	pub kept_count: u64,
	/// Where they are: the root renamed aside, or the root itself when it could not be renamed;
	/// `None` when they are nowhere the helper knows of (moved or deleted since).
	#[serde(serialize_with = "lossy_path")]
	pub kept_in: Option<PathBuf>,
}

/// What `unregister` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
// Tradeoff: each flag is a field of the reply the parent reads on its own; no two of them make a
// state the others could stand for.
#[expect(clippy::struct_excessive_bools)]
pub struct UnregisterReport {
	/// Whether the root is no longer registered.
	pub unregistered: bool,
	/// How many clean downloaded files were deleted.
	pub deleted_hydrated: u64,
	/// The first [`MAX_LIST_ENTRIES`] items kept.
	pub kept: Vec<KeptItem>,
	/// How many items were kept: those `kept` names, or, for a root the OS no longer held as
	/// registered when the unregister began (one stopped part way), every item but a directory
	/// left in it, as any of them may hold a change the drive never had.
	pub kept_count: u64,
	/// Where what stayed is: the root renamed aside, or the root itself when it could not be
	/// renamed; `None` when nothing stayed. Derived from the root's path, so UTF-8.
	#[serde(serialize_with = "lossy_path")]
	pub kept_in: Option<PathBuf>,
	/// Whether the root directory is gone.
	pub root_removed: bool,
	/// Whether the user's sealing key was deleted.
	pub key_deleted: bool,
	/// Whether the user's data directory was wiped: the sealed session and what the cache kept.
	/// When it was not (an elevated run, or a failed wipe), the index keeps the root, so a later
	/// unregister run as the user wipes it.
	pub data_deleted: bool,
	/// Names what `kept` and `keptCount` report, when anything was kept: a later reply reports it
	/// again under this id if the helper is killed before the parent's next request.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub kept_id: Option<KeptId>,
	/// What earlier unregisters of the root kept and no reply the parent answered reported; left
	/// out when empty.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub kept_earlier: Vec<KeptEarlier>,
}

impl UnregisterReport {
	/// Every kept report this one carries.
	pub(crate) fn kept_ids(&self) -> Vec<KeptId> {
		self.kept_id
			.into_iter()
			.chain(self.kept_earlier.iter().map(|earlier| earlier.id))
			.collect()
	}
}

/// One root `listRoots` lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootListing {
	/// Whose root it is.
	#[serde(rename = "userId")]
	pub user: UserId,
	/// Where it is.
	pub root: RootDir,
	/// Whether the OS holds a registration of it by this provider.
	pub registered: bool,
	/// Whether this helper serves it with a session, connected to the OS.
	pub serving: bool,
	/// What unregisters of the root kept and no reply the parent answered reported, as in a
	/// `register` or `unregister` reply; left out when empty.
	#[serde(rename = "keptEarlier", skip_serializing_if = "Vec::is_empty")]
	pub kept_earlier: Vec<KeptEarlier>,
}

/// What `getState` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateReport {
	/// The helper's state.
	pub state: PhaseName,
	/// Whose root is registered or served.
	#[serde(rename = "userId")]
	pub user: Option<UserId>,
	/// Which root.
	pub root: Option<RootDir>,
	/// Whether the root's watcher runs.
	pub watching: bool,
	/// How many items in the root hold what the drive does not have yet, while serving;
	/// `null` when the helper cannot say (not serving, or serving but not done looking at the
	/// root yet), which means there may be some.
	pub pending_uploads: Option<u64>,
}

/// A successful reply's value, the object each variant spells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ReplyValue {
	/// `register`: the state reached, whether a sealed session was resumed, and what earlier
	/// unregisters of the root kept that no reply the parent answered reported (left out when
	/// none).
	Registered {
		/// Registered, or serving after a resume.
		state: PhaseName,
		/// Whether a sealed session was resumed.
		resumed: bool,
		/// What earlier unregisters kept.
		#[serde(rename = "keptEarlier", skip_serializing_if = "Vec::is_empty")]
		kept_earlier: Vec<KeptEarlier>,
	},
	/// `setAuth`: serving; `adopted` when the root was served already and the cache takes the new
	/// session on its own.
	#[serde(serialize_with = "serving_value")]
	Serving {
		/// Whether the session went to a root already served.
		adopted: bool,
	},
	/// `clearAuth`: whether the user was known.
	Cleared {
		/// Whether there was anything to clear.
		cleared: bool,
	},
	/// `unregister`.
	Unregistered(UnregisterReport),
	/// `listRoots`.
	Roots {
		/// Every root the index names.
		roots: Vec<RootListing>,
	},
	/// `getState`.
	State(StateReport),
	/// `shutdown`: an empty object.
	Empty {},
}

/// A failed reply's error. `message` is English, for logs, and holds no secret; the wire carries
/// at most [`MAX_MESSAGE_CHARS`] characters of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplyError {
	/// Why, for the parent to branch on.
	pub code: ErrorCode,
	/// What happened, for logs.
	#[serde(serialize_with = "bounded")]
	pub message: String,
}

impl ReplyError {
	/// An error of `code` saying `message`.
	pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
		Self {
			code,
			message: message.into(),
		}
	}
}

impl From<ParseError> for ReplyError {
	fn from(error: ParseError) -> Self {
		Self::new(error.code, error.message)
	}
}

/// What an `activity` entry says happened, spelled in camelCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityKind {
	/// A local file reached the drive.
	Uploaded,
	/// A local item was found equal to a remote one.
	Adopted,
	/// A local directory was created on the drive.
	DirCreated,
	/// A local delete moved the remote item to the trash.
	Trashed,
	/// A local rename or move reached the drive.
	Moved,
	/// A local delete went ahead while the remote file got a newer version, which stays.
	RemoteOutlivedDelete,
	/// A file is held open for writing; it is uploaded once closed.
	Busy,
	/// A change failed repeatedly and waits.
	Parked,
	/// A local delete was vetoed.
	DeleteVetoed,
	/// A local rename was vetoed.
	RenameVetoed,
	/// An allowed rename failed locally and was undone on the drive.
	RenameUndone,
	/// A remote item appeared.
	RemoteCreated,
	/// A remote file got a new version.
	RemoteUpdated,
	/// A remote item was renamed or moved.
	RemoteMoved,
	/// A remote item went, and so did its clean placeholder.
	RemoteRemoved,
	/// A remote item went, and its local item, holding data the drive lacks, was kept.
	KeptLocal,
	/// A remote directory went, and its local directory was kept for what it holds.
	DirKept,
	/// A file's bytes on disk did not match the drive's hash and were removed.
	Damaged,
	/// A file's space was freed.
	Freed,
	/// A file asked to be freed holds local changes and was kept.
	KeptDownloaded,
}

/// One entry of an `activity` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivityEntry {
	/// What happened.
	pub kind: ActivityKind,
	/// To which path below the root (where it was, for a move).
	pub path: String,
	/// Where a move went.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub to: Option<String>,
	/// Why, for a parked change or a veto.
	#[serde(
		serialize_with = "failure_value",
		skip_serializing_if = "Option::is_none"
	)]
	pub failure: Option<Failure>,
}

/// Where a skipped name lies: the entry's `path` or its `dirUuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SkippedPlace {
	/// A local item, by its path below the root.
	#[serde(rename = "path")]
	Path(String),
	/// A remote child of the directory with this uuid.
	#[serde(rename = "dirUuid")]
	Dir(Uuid),
}

/// One entry of a `skippedNames` line: a name the provider does not sync, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedEntry {
	/// Where it lies.
	#[serde(flatten)]
	pub place: SkippedPlace,
	/// The name; `None` for a remote item whose name does not decrypt.
	pub name: Option<String>,
	/// Why, as the wire spells it.
	pub reason: &'static str,
}

/// The helper's state, as a `status` line reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEvent {
	/// The state.
	pub state: PhaseName,
	/// Whose root.
	#[serde(rename = "userId")]
	pub user: Option<UserId>,
	/// Which root.
	pub root: Option<RootDir>,
	/// Whether the root's watcher runs.
	pub watching: bool,
}

/// What `--unregister-all` did for one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnregisterAllEntry {
	/// Whose root.
	#[serde(rename = "userId")]
	pub user: UserId,
	/// Which root.
	pub root: RootDir,
	/// What unregistering it did, as `ok`, or why it failed, as `error`.
	#[serde(flatten, serialize_with = "report_or_error")]
	pub outcome: Result<UnregisterReport, ReplyError>,
	/// Whether the user's sealing key was left, as an elevated run cannot delete it.
	pub key_left: bool,
}

/// The `hello` line's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hello {
	/// The protocol's version, [`PROTOCOL_VERSION`].
	protocol: u32,
	/// The helper's version, its crate's.
	version: &'static str,
	/// The Cloud Files platform's build number, when known.
	build: Option<u32>,
}

impl Hello {
	/// The `hello` of this helper, on the Cloud Files platform build `build`.
	pub fn new(build: Option<u32>) -> Self {
		Self {
			protocol: PROTOCOL_VERSION,
			version: env!("CARGO_PKG_VERSION"),
			build,
		}
	}
}

/// A line the helper writes, named by its `t`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum OutMessage {
	/// The first line of a helper that starts.
	Hello(Hello),
	/// Why the helper refused to start; the only line, written instead of `hello`.
	Fatal {
		/// Why.
		code: FatalCode,
		/// What happened, for logs; at most [`MAX_MESSAGE_CHARS`] characters of it go out.
		#[serde(serialize_with = "bounded")]
		message: String,
	},
	/// The answer to the request `id`: `ok` and its `value`, or its `error`.
	Reply {
		/// The request's id.
		id: RequestId,
		/// Its value or its error.
		#[serde(flatten, serialize_with = "value_or_error")]
		outcome: Result<ReplyValue, ReplyError>,
	},
	/// The helper's state changed.
	Status(StatusEvent),
	/// What the provider did, at most [`MAX_LIST_ENTRIES`] entries, with how many were dropped
	/// since the last line because the queue was full.
	Activity {
		/// What happened, in order.
		entries: Vec<ActivityEntry>,
		/// How many entries were dropped; left out when none was.
		#[serde(skip_serializing_if = "is_zero")]
		dropped: u64,
	},
	/// Both sides of a conflict were kept.
	Conflict {
		/// Where the remote side is.
		path: String,
		/// Where the local side was kept.
		kept_as: String,
		/// Which two versions were kept.
		#[serde(serialize_with = "conflict_value")]
		kind: ConflictKind,
	},
	/// Names the provider does not sync, at most [`MAX_LIST_ENTRIES`], with how many were dropped
	/// since the last line because the queue was full.
	SkippedNames {
		/// The names.
		entries: Vec<SkippedEntry>,
		/// How many names were dropped; left out when none was.
		#[serde(skip_serializing_if = "is_zero")]
		dropped: u64,
	},
	/// How many items in the root hold what the drive does not have yet: edits and new files
	/// waiting, held by a writer, on their way up, failed or parked, new directories, conflict
	/// copies. Sent once the helper can say, and again whenever it changes.
	PendingUploads {
		/// How many.
		count: u64,
	},
	/// The served session stopped being accepted.
	AuthRevoked {
		/// Whose session.
		#[serde(rename = "userId")]
		user: UserId,
	},
	/// A line that was not a request and carried no id.
	ProtocolError {
		/// Why.
		code: ErrorCode,
	},
	/// What `--unregister-all` did.
	UnregisterAll {
		/// Each root.
		roots: Vec<UnregisterAllEntry>,
	},
}

/// The wire's spelling of `failure`, which the core's type leaves to the protocol.
fn failure_value<S: Serializer>(failure: &Option<Failure>, s: S) -> Result<S::Ok, S::Error> {
	let Some(failure) = failure else {
		return s.serialize_none();
	};
	s.serialize_str(match failure {
		Failure::NotAuthenticated => "notAuthenticated",
		Failure::NotFound => "notFound",
		Failure::Unreachable => "unreachable",
		Failure::Unsupported => "unsupported",
		Failure::ContentDamaged => "contentDamaged",
		Failure::Undecryptable => "undecryptable",
		Failure::StalePlaceholder => "stalePlaceholder",
		Failure::InvalidRequest => "invalidRequest",
		Failure::Transient => "transient",
		Failure::Internal => "internal",
	})
}

/// The wire's spelling of `kind`, which the core's type leaves to the protocol.
fn conflict_value<S: Serializer>(kind: &ConflictKind, s: S) -> Result<S::Ok, S::Error> {
	s.serialize_str(match kind {
		ConflictKind::EditVsEdit => "editVsEdit",
		ConflictKind::CreateVsExisting => "createVsExisting",
	})
}

/// `path` as a string: the protocol's paths came in as UTF-8, and so do those built from them.
fn lossy_path<S: Serializer>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
	match path {
		Some(path) => s.serialize_str(&path.to_string_lossy()),
		None => s.serialize_none(),
	}
}

/// `setAuth`'s value: always serving.
fn serving_value<S: Serializer>(adopted: &bool, s: S) -> Result<S::Ok, S::Error> {
	let mut value = s.serialize_struct("Serving", 2)?;
	value.serialize_field("state", &PhaseName::Serving)?;
	value.serialize_field("adopted", adopted)?;
	value.end()
}

/// A reply's outcome: `ok: true` and its `value`, or `ok: false` and its `error`.
fn value_or_error<S: Serializer>(
	outcome: &Result<ReplyValue, ReplyError>,
	s: S,
) -> Result<S::Ok, S::Error> {
	let mut fields = s.serialize_struct("Outcome", 2)?;
	match outcome {
		Ok(value) => {
			fields.serialize_field("ok", &true)?;
			fields.serialize_field("value", value)?;
		}
		Err(error) => {
			fields.serialize_field("ok", &false)?;
			fields.serialize_field("error", error)?;
		}
	}
	fields.end()
}

/// An `--unregister-all` root's outcome: its report as `ok`, or its `error`.
fn report_or_error<S: Serializer>(
	outcome: &Result<UnregisterReport, ReplyError>,
	s: S,
) -> Result<S::Ok, S::Error> {
	let mut fields = s.serialize_struct("Outcome", 1)?;
	match outcome {
		Ok(report) => fields.serialize_field("ok", report)?,
		Err(error) => fields.serialize_field("error", error)?,
	}
	fields.end()
}

fn is_zero(count: &u64) -> bool {
	*count == 0
}

/// `message` cut to [`MAX_MESSAGE_CHARS`] characters, the last of a cut one `…`.
fn bounded<S: Serializer>(message: &str, s: S) -> Result<S::Ok, S::Error> {
	if message.chars().nth(MAX_MESSAGE_CHARS).is_none() {
		return s.serialize_str(message);
	}
	let mut cut: String = message.chars().take(MAX_MESSAGE_CHARS - 1).collect();
	cut.push('…');
	s.serialize_str(&cut)
}

impl OutMessage {
	/// The message as one JSON line, without its newline.
	pub fn to_line(&self) -> String {
		serde_json::to_string(self).expect(
			"a protocol message holds only strings, numbers and maps with string keys (should be \
			 impossible)",
		)
	}
}

#[cfg(test)]
mod tests;
