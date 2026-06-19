use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use crate::{crypto::EncryptedString, fs::UuidStr, traits::CowHelpers};

pub const ENDPOINT: &str = "v3/user/events";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub filter: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

/// Per-event deserialization failure. Stored alongside successfully-parsed
/// events in the response so that one malformed/unknown variant doesn't fail
/// the whole list.
#[derive(Debug, Clone)]
pub struct UserEventDeserializeError {
	pub message: String,
	pub raw: String,
}

impl std::fmt::Display for UserEventDeserializeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.message)
	}
}

impl std::error::Error for UserEventDeserializeError {}

/// Response for `POST /v3/user/events`. Each event is presented as a
/// `Result` so individual malformed/unknown events can be inspected (or
/// skipped) without losing the rest.
#[derive(Deserialize, Debug)]
pub struct Response<'a> {
	#[serde(deserialize_with = "deserialize_events")]
	pub events: Vec<Result<UserEvent<'a>, UserEventDeserializeError>>,
}

/// Borrows each element as a `&RawValue` (zero heap allocation per event),
/// then re-parses it as `UserEvent<'static>`; failures are captured as `Err`.
fn deserialize_events<'de, D>(
	deserializer: D,
) -> Result<Vec<Result<UserEvent<'static>, UserEventDeserializeError>>, D::Error>
where
	D: Deserializer<'de>,
{
	struct EventsVisitor;

	impl<'de> Visitor<'de> for EventsVisitor {
		type Value = Vec<Result<UserEvent<'static>, UserEventDeserializeError>>;

		fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
			f.write_str("a sequence of user events")
		}

		fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
			let mut results = Vec::with_capacity(seq.size_hint().unwrap_or(0));
			while let Some(raw) = seq.next_element::<&'de RawValue>()? {
				match serde_json::from_str::<UserEvent<'static>>(raw.get()) {
					Ok(event) => results.push(Ok(event)),
					Err(e) => results.push(Err(UserEventDeserializeError {
						message: e.to_string(),
						raw: raw.get().to_string(),
					})),
				}
			}
			Ok(results)
		}
	}

	deserializer.deserialize_seq(EventsVisitor)
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct UserEvent<'a> {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub uuid: UuidStr,
	#[serde(flatten)]
	pub kind: UserEventKind<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(tag = "type", content = "info", rename_all = "camelCase")]
pub enum UserEventKind<'a> {
	FileUploaded(FileMetadataInfo<'a>),
	FileVersioned(FileMetadataInfo<'a>),
	FileRestored(FileMetadataInfo<'a>),
	VersionedFileRestored(FileMetadataInfo<'a>),
	FileMoved(FileMetadataInfo<'a>),
	FileRenamed(FileMetadataPairInfo<'a>),
	FileMetadataChanged(FileMetadataPairInfo<'a>),
	FileTrash(FileMetadataInfo<'a>),
	FileRm(FileMetadataInfo<'a>),
	FileShared(FileSharedInfo<'a>),
	FileLinkEdited(FileMetadataInfo<'a>),
	DeleteFilePermanently(FileMetadataInfo<'a>),

	FolderTrash(FolderNameInfo<'a>),
	FolderShared(FolderSharedInfo<'a>),
	FolderMoved(FolderNameInfo<'a>),
	FolderRenamed(FolderNamePairInfo<'a>),
	FolderMetadataChanged(FolderNamePairInfo<'a>),
	SubFolderCreated(FolderNameInfo<'a>),
	BaseFolderCreated(FolderNameInfo<'a>),
	FolderRestored(FolderNameInfo<'a>),
	FolderColorChanged(FolderNameInfo<'a>),
	DeleteFolderPermanently(FolderNameInfo<'a>),

	Login(BaseInfo<'a>),
	FailedLogin(BaseInfo<'a>),
	PasswordChanged(BaseInfo<'a>),
	#[serde(rename = "2faEnabled")]
	TwoFaEnabled(BaseInfo<'a>),
	#[serde(rename = "2faDisabled")]
	TwoFaDisabled(BaseInfo<'a>),
	RequestAccountDeletion(BaseInfo<'a>),
	TrashEmptied(BaseInfo<'a>),
	DeleteAll(BaseInfo<'a>),
	DeleteVersioned(BaseInfo<'a>),
	DeleteUnfinished(BaseInfo<'a>),

	CodeRedeemed(CodeRedeemedInfo<'a>),
	EmailChanged(EmailChangedInfo<'a>),
	EmailChangeAttempt(EmailChangeAttemptInfo<'a>),
	RemovedSharedInItems(RemovedSharedInItemsInfo<'a>),
	RemovedSharedOutItems(RemovedSharedOutItemsInfo<'a>),
	FolderLinkEdited(FolderLinkEditedInfo<'a>),
	ItemFavorite(ItemFavoriteInfo<'a>),
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct BaseInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadataInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadataPairInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	pub old_metadata: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileSharedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	pub receiver_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderNameInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderNamePairInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	pub old_name: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderSharedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	pub receiver_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct CodeRedeemedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub code: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct EmailChangedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct EmailChangeAttemptInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub email: Cow<'a, str>,
	pub new_email: Cow<'a, str>,
	pub old_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct RemovedSharedInItemsInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub count: u64,
	pub sharer_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct RemovedSharedOutItemsInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub count: u64,
	pub receiver_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderLinkEditedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(rename = "linkUUID")]
	pub link_uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ItemFavoriteInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::boolean::number")]
	pub value: bool,
	pub metadata: EncryptedString<'a>,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn login_event_json(id: u64) -> String {
		format!(
			r#"{{"id":{id},"timestamp":1700000,"uuid":"11111111-1111-1111-1111-111111111111","type":"login","info":{{"ip":"1.2.3.4","userAgent":"ua"}}}}"#
		)
	}

	#[test]
	fn known_event_deserializes_to_ok() {
		let json = format!(r#"{{"events":[{}]}}"#, login_event_json(1));
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
		let event = resp.events[0].as_ref().expect("login should parse");
		assert_eq!(event.id, 1);
		assert!(matches!(event.kind, UserEventKind::Login(_)));
	}

	#[test]
	fn unknown_event_type_is_captured_as_err() {
		let raw = r#"{"id":2,"timestamp":1700000,"uuid":"22222222-2222-2222-2222-222222222222","type":"futureVariantWeDontKnowAbout","info":{"ip":"1.2.3.4","userAgent":"ua"}}"#;
		let json = format!(r#"{{"events":[{raw}]}}"#);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
		let err = resp
			.events
			.into_iter()
			.next()
			.unwrap()
			.expect_err("unknown variant should land in Err");
		// Raw JSON of the failing event must be preserved verbatim for diagnostics.
		assert_eq!(err.raw, raw);
		assert!(!err.message.is_empty(), "error message should be populated");
	}

	#[test]
	fn malformed_event_does_not_poison_neighbours() {
		// First and third events are valid logins; second is malformed (missing
		// `info` for a known variant). The list must still contain three
		// entries, with Ok/Err/Ok in order.
		let malformed = r#"{"id":99,"timestamp":1700000,"uuid":"00000000-0000-0000-0000-000000000000","type":"login"}"#;
		let json = format!(
			r#"{{"events":[{},{malformed},{}]}}"#,
			login_event_json(1),
			login_event_json(3)
		);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 3);
		assert!(resp.events[0].is_ok(), "first event should parse");
		let err = resp.events[1]
			.as_ref()
			.expect_err("middle event should be Err");
		assert_eq!(err.raw, malformed);
		assert!(resp.events[2].is_ok(), "last event should parse");
	}

	#[test]
	fn empty_events_array_yields_empty_vec() {
		let json = r#"{"events":[]}"#;
		let resp: Response = serde_json::from_str(json).unwrap();
		assert!(resp.events.is_empty());
	}

	#[test]
	fn unknown_top_level_fields_are_ignored() {
		// Forward-compat: if the server adds e.g. a `cursor` field at the top
		// level, our deserializer must not fail.
		let json = format!(
			r#"{{"cursor":"abc","events":[{}],"extra":42}}"#,
			login_event_json(1)
		);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
	}
}
