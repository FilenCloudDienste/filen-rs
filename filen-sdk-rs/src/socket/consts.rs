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
pub(super) const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
pub(super) const PING_INTERVAL: Duration = Duration::from_secs(15);

pub(super) const WEBSOCKET_URL_CORE: &str =
	"wss://socket.filen.io/socket.io/?EIO=3&transport=websocket&t=";

pub(super) const AUTHED_TRUE: &str = r#"["authed",true]"#;
pub(super) const VERSIONED_EVENT_PREFIXES: &[&str] =
	&[r#"["file-versioned","#, r#"["fileVersioned","#];
