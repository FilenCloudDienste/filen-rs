use std::time::Duration;

use filen_types::api::v3::socket::{MessageType, PacketType};

pub(super) const MESSAGE_EVENT_PAYLOAD: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Event as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create handshake payload string"),
	};

pub(super) const MESSAGE_CONNECT_PAYLOAD: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Connect as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create handshake payload string"),
	};

/// The prefix of an ack frame: `43<id>[<args...>]`.
pub(super) const MESSAGE_ACK_PREFIX: &str =
	match str::from_utf8(&[PacketType::Message as u8, MessageType::Ack as u8]) {
		Ok(s) => s,
		Err(_) => panic!("Failed to create ack prefix string"),
	};

pub(super) const PING_MESSAGE: &str = match str::from_utf8(&[PacketType::Ping as u8]) {
	Ok(s) => s,
	Err(_) => panic!("Failed to create ping message string"),
};

/// The pause before reconnecting after a session ended, and the first one after a failed attempt.
pub(super) const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// The longest pause between attempts to connect that keep failing (see `ReconnectBackoff`): a
/// rejected certificate or a proxy that refuses the socket fails every time, and a retry each
/// second would load the server or proxy, the battery and the log for as long as the app runs.
pub(super) const RECONNECT_MAX: Duration = Duration::from_secs(60);
pub(super) const PING_INTERVAL: Duration = Duration::from_secs(15);
/// How many frames the connected loop can queue for the writer task before queueing fails.
pub(super) const OUTBOUND_FRAME_CAPACITY: usize = 64;
/// Ceiling for one full connection setup (websocket connect + socket.io
/// handshake + auth). Without it, a black-holed TCP connect or a handshake
/// read against a dead peer parks the websocket task forever with no retry.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a `lockHello` probe waits for its ack before the connection uses HTTP for locks. The
/// server defines no timeout; a server without socket locks never answers.
pub(super) const LOCK_HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a `lock` event waits for its ack before its outcome is taken as unknown, and the
/// operation goes to HTTP.
pub(super) const LOCK_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a lock operation waits for its connection's reply: the ack timeout, plus a margin for
/// a connection loop that is slow to notice it ran out. A loop that never replies (its runtime
/// shutting down under it) then costs the operation this long, not forever.
pub(super) const LOCK_REPLY_TIMEOUT: Duration = Duration::from_secs(11);
/// How many `lock` events a connection may have unanswered, those it gave up on included: the
/// server refuses more than this many on one socket, and counts one it has not answered whatever
/// the client gave up on.
pub(super) const LOCK_MAX_IN_FLIGHT: usize = 32;
/// After how many `lock` acks that did not come in time a connection stops carrying lock
/// operations: a server that answers them that late is no faster than HTTP, and each one costs
/// its operation [`LOCK_ACK_TIMEOUT`].
pub(super) const LOCK_DEGRADE_AFTER: usize = 2;

pub(super) const WEBSOCKET_URL_CORE: &str =
	"wss://socket.filen.io/socket.io/?EIO=3&transport=websocket&t=";

pub(super) const AUTHED_TRUE: &str = r#"["authed",true]"#;
pub(super) const ARCHIVED_EVENT_PREFIX: &str = r#"["file-archived","#;
