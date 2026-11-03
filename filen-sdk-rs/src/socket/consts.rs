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

pub(super) const PING_MESSAGE: &str = match str::from_utf8(&[PacketType::Ping as u8]) {
	Ok(s) => s,
	Err(_) => panic!("Failed to create ping message string"),
};

pub(super) const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// The longest pause between attempts to connect through a proxy that keeps failing (see
/// `ProxyBackoff`): a proxy that refuses the socket refuses it every time, and a retry each
/// second would hammer it, and fill the log, for as long as the app runs.
pub(super) const PROXY_RECONNECT_MAX: Duration = Duration::from_secs(60);
pub(super) const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Ceiling for one full connection setup (websocket connect + socket.io
/// handshake + auth). Without it, a black-holed TCP connect or a handshake
/// read against a dead peer parks the websocket task forever with no retry.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) const WEBSOCKET_URL_CORE: &str =
	"wss://socket.filen.io/socket.io/?EIO=3&transport=websocket&t=";

pub(super) const AUTHED_TRUE: &str = r#"["authed",true]"#;
pub(super) const ARCHIVED_EVENT_PREFIX: &str = r#"["file-archived","#;
