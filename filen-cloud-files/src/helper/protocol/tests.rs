use std::path::{Path, PathBuf};

use filen_types::fs::Uuid;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

use super::*;
use crate::helper::identify::{NOT_FOUND, NOT_IN_ROOT, NOT_SERVING, NOT_SYNCED, UNKNOWN_ITEM};

/// The golden fixtures the desktop app reads too.
const FIXTURES: &str = include_str!(concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/tests/fixtures/protocol/v1.jsonl"
));

/// Every fixture case going `dir`, as (case, line, expect).
fn cases(dir: &str) -> Vec<(String, Value, Value)> {
	FIXTURES
		.lines()
		.map(|line| serde_json::from_str::<Value>(line).unwrap())
		.filter(|case| case["dir"] == dir)
		.map(|case| {
			(
				case["case"].as_str().unwrap().to_owned(),
				case["line"].clone(),
				case["expect"].clone(),
			)
		})
		.collect()
}

/// The request type a parsed message is.
fn request_type(request: &Request) -> &'static str {
	match request {
		Request::Register(_) => "register",
		Request::SetAuth { .. } => "setAuth",
		Request::ClearAuth { .. } => "clearAuth",
		Request::Unregister { .. } => "unregister",
		Request::Identify { .. } => "identify",
		Request::ListRoots => "listRoots",
		Request::GetState => "getState",
		Request::Shutdown => "shutdown",
	}
}

#[test]
fn every_input_fixture_parses_or_is_refused_as_it_says() {
	let cases = cases("in");
	assert!(cases.len() > 30);
	for (case, line, expect) in cases {
		let text = match line {
			Value::String(raw) => raw,
			other => other.to_string(),
		};
		match (parse_line(&text), expect.get("t")) {
			(Ok(message), Some(t)) => assert_eq!(request_type(&message.request), t, "{case}"),
			(Err(error), None) => {
				assert_eq!(
					serde_json::to_value(error.code).unwrap(),
					expect["code"],
					"{case}"
				);
				assert_eq!(
					error.id.map(RequestId::get),
					expect["id"].as_u64(),
					"{case}"
				);
			}
			(outcome, _) => panic!("{case}: expected {expect}, got {outcome:?}"),
		}
	}
}

/// A report of everything an unregister can say.
fn report() -> UnregisterReport {
	UnregisterReport {
		unregistered: true,
		deleted_hydrated: 3,
		kept: vec![
			KeptItem {
				path: "a\\b.txt".to_owned(),
				reason: KeptItemReason::Modified,
			},
			KeptItem {
				path: "new.txt".to_owned(),
				reason: KeptItemReason::NotUploaded,
			},
		],
		kept_count: 2,
		kept_in: Some(PathBuf::from("C:\\Users\\u\\Filen (not synced 2026-11-12)")),
		root_removed: false,
		key_deleted: true,
		data_deleted: true,
		kept_id: None,
		kept_earlier: Vec::new(),
	}
}

fn id(n: u64) -> RequestId {
	RequestId::new(n).unwrap()
}

fn root() -> RootDir {
	RootDir::parse("C:\\Users\\u\\Filen".to_owned()).unwrap()
}

fn reply(n: u64, value: ReplyValue) -> OutMessage {
	OutMessage::Reply {
		id: id(n),
		outcome: Ok(value),
	}
}

/// The message each output fixture case spells.
fn out_message(case: &str) -> OutMessage {
	let user = Some(UserId::from(42));
	match case {
		"hello" => OutMessage::Hello(Hello::new(Some(26300))),
		"fatal" => OutMessage::Fatal {
			code: FatalCode::Elevated,
			message: "the helper runs elevated".to_owned(),
		},
		"reply register" => reply(
			1,
			ReplyValue::Registered {
				state: PhaseName::Registered,
				resumed: false,
				kept_earlier: Vec::new(),
				branded: None,
			},
		),
		"reply register resumed" => reply(
			1,
			ReplyValue::Registered {
				state: PhaseName::Serving,
				resumed: true,
				kept_earlier: Vec::new(),
				branded: None,
			},
		),
		"reply register keptEarlier" => reply(
			1,
			ReplyValue::Registered {
				state: PhaseName::Serving,
				resumed: true,
				kept_earlier: vec![KeptEarlier {
					id: KeptId::from(0xab),
					kept: vec![KeptItem {
						path: "new.txt".to_owned(),
						reason: KeptItemReason::NotUploaded,
					}],
					kept_count: 1,
					kept_in: Some(PathBuf::from("C:\\Users\\u\\Filen (not synced 2026-11-12)")),
				}],
				branded: None,
			},
		),
		"reply register branded" => reply(
			1,
			ReplyValue::Registered {
				state: PhaseName::Serving,
				resumed: true,
				kept_earlier: Vec::new(),
				branded: Some(false),
			},
		),
		"reply setAuth" => reply(
			2,
			ReplyValue::Serving {
				adopted: true,
				branded: None,
			},
		),
		"reply setAuth branded" => reply(
			2,
			ReplyValue::Serving {
				adopted: false,
				branded: Some(true),
			},
		),
		"reply clearAuth" => reply(3, ReplyValue::Cleared { cleared: false }),
		"reply unregister" => reply(4, ReplyValue::Unregistered(report())),
		"reply unregister keptEarlier" => reply(
			4,
			ReplyValue::Unregistered(UnregisterReport {
				kept_in: Some(PathBuf::from(
					"C:\\Users\\u\\Filen (not synced 2026-11-12) 2",
				)),
				kept_id: Some(KeptId::from(0x0123_4567_89ab_cdef)),
				kept_earlier: vec![
					KeptEarlier {
						id: KeptId::from(0xab),
						kept: Vec::new(),
						kept_count: 3,
						kept_in: Some(PathBuf::from("C:\\Users\\u\\Filen (not synced 2026-11-12)")),
					},
					KeptEarlier {
						id: KeptId::from(u64::MAX),
						kept: Vec::new(),
						kept_count: 1,
						kept_in: None,
					},
				],
				..report()
			}),
		),
		"reply listRoots" => reply(
			5,
			ReplyValue::Roots {
				roots: vec![RootListing {
					user: UserId::from(42),
					root: root(),
					registered: true,
					serving: true,
					kept_earlier: Vec::new(),
				}],
			},
		),
		"reply listRoots keptEarlier" => reply(
			5,
			ReplyValue::Roots {
				roots: vec![RootListing {
					user: UserId::from(42),
					root: root(),
					registered: false,
					serving: false,
					kept_earlier: vec![KeptEarlier {
						id: KeptId::from(0xab),
						kept: Vec::new(),
						kept_count: 2,
						kept_in: Some(PathBuf::from("C:\\Users\\u\\Filen")),
					}],
				}],
			},
		),
		"reply getState" => reply(
			6,
			ReplyValue::State(StateReport {
				state: PhaseName::Serving,
				user,
				root: Some(root()),
				watching: true,
				pending_uploads: Some(2),
			}),
		),
		"reply getState idle" => reply(
			6,
			ReplyValue::State(StateReport {
				state: PhaseName::Idle,
				user: None,
				root: None,
				watching: false,
				pending_uploads: None,
			}),
		),
		"reply shutdown" => reply(7, ReplyValue::Empty {}),
		"reply error" => OutMessage::Reply {
			id: id(8),
			outcome: Err(ReplyError::new(
				ErrorCode::RootInUse,
				"the root is another user's",
			)),
		},
		"status" => OutMessage::Status(StatusEvent {
			state: PhaseName::Serving,
			user,
			root: Some(root()),
			watching: true,
		}),
		"activity" => OutMessage::Activity {
			entries: vec![
				ActivityEntry {
					kind: ActivityKind::Uploaded,
					path: "a.txt".to_owned(),
					to: None,
					failure: None,
				},
				ActivityEntry {
					kind: ActivityKind::Moved,
					path: "a.txt".to_owned(),
					to: Some("b\\a.txt".to_owned()),
					failure: None,
				},
				ActivityEntry {
					kind: ActivityKind::Parked,
					path: "c.txt".to_owned(),
					to: None,
					failure: Some(Failure::Unreachable),
				},
			],
			dropped: 4,
		},
		"activity without drops" => OutMessage::Activity {
			entries: vec![ActivityEntry {
				kind: ActivityKind::RemoteCreated,
				path: "d".to_owned(),
				to: None,
				failure: None,
			}],
			dropped: 0,
		},
		"conflict" => OutMessage::Conflict {
			path: "a.txt".to_owned(),
			kept_as: "a (conflict).txt".to_owned(),
			kind: ConflictKind::EditVsEdit,
		},
		"skippedNames" => OutMessage::SkippedNames {
			entries: vec![
				SkippedEntry {
					place: SkippedPlace::Path("x\\bad.".to_owned()),
					name: Some("bad.".to_owned()),
					reason: "invalidName",
				},
				SkippedEntry {
					place: SkippedPlace::Dir(Uuid::from_u128(0x100)),
					name: None,
					reason: "undecryptable",
				},
			],
			dropped: 2,
		},
		"pendingUploads" => OutMessage::PendingUploads { count: 5 },
		"authRevoked" => OutMessage::AuthRevoked {
			user: UserId::from(42),
		},
		"protocolError" => OutMessage::ProtocolError {
			code: ErrorCode::LineTooLong,
		},
		"unregisterAll" => OutMessage::UnregisterAll {
			roots: vec![
				UnregisterAllEntry {
					user: UserId::from(42),
					root: root(),
					outcome: Ok(UnregisterReport {
						unregistered: true,
						deleted_hydrated: 0,
						kept: Vec::new(),
						kept_count: 0,
						kept_in: None,
						root_removed: true,
						key_deleted: false,
						data_deleted: false,
						kept_id: None,
						kept_earlier: Vec::new(),
					}),
					key_left: true,
					branding_left: false,
				},
				UnregisterAllEntry {
					user: UserId::from(43),
					root: RootDir::parse("D:\\Filen".to_owned()).unwrap(),
					outcome: Err(ReplyError::new(ErrorCode::UnsyncedChanges, "m")),
					key_left: false,
					branding_left: false,
				},
			],
		},
		"unregisterAll brandingLeft" => OutMessage::UnregisterAll {
			roots: vec![UnregisterAllEntry {
				user: UserId::from(42),
				root: root(),
				outcome: Ok(UnregisterReport {
					unregistered: true,
					deleted_hydrated: 0,
					kept: Vec::new(),
					kept_count: 0,
					kept_in: None,
					root_removed: true,
					key_deleted: false,
					data_deleted: false,
					kept_id: None,
					kept_earlier: Vec::new(),
				}),
				key_left: true,
				branding_left: true,
			}],
		},
		"reply identify file" => reply(
			9,
			ReplyValue::Identified(IdentifiedItem::File {
				uuid: Uuid::from_u128(0x303),
				stable_uuid: Uuid::from_u128(0x302),
				parents: vec![Uuid::from_u128(0x201), Uuid::from_u128(0x202)],
			}),
		),
		"reply identify dir" => reply(
			9,
			ReplyValue::Identified(IdentifiedItem::Dir {
				uuid: Uuid::from_u128(0x202),
				parents: vec![Uuid::from_u128(0x201)],
			}),
		),
		"reply identify root" => reply(
			9,
			ReplyValue::Identified(IdentifiedItem::Root {
				uuid: Uuid::from_u128(0x100),
			}),
		),
		"reply error notInRoot" => identify_error(ErrorCode::NotInRoot, NOT_IN_ROOT),
		"reply error notSynced" => identify_error(ErrorCode::NotSynced, NOT_SYNCED),
		"reply error unknownItem" => identify_error(ErrorCode::UnknownItem, UNKNOWN_ITEM),
		"reply error notServing" => identify_error(ErrorCode::NotServing, NOT_SERVING),
		"reply error notFound" => identify_error(ErrorCode::NotFound, NOT_FOUND),
		other => panic!("no message for the output case {other}"),
	}
}

#[derive(Deserialize)]
struct RawCase<'a> {
	case: String,
	dir: String,
	#[serde(borrow)]
	line: &'a RawValue,
}

#[test]
fn every_output_fixture_is_byte_for_byte_what_the_helper_writes() {
	let mut checked = 0;
	for fixture in FIXTURES.lines() {
		let case: RawCase<'_> = serde_json::from_str(fixture).unwrap();
		if case.dir != "out" {
			continue;
		}
		let expected = case
			.line
			.get()
			.replace("<crate version>", env!("CARGO_PKG_VERSION"));
		let line = out_message(&case.case).to_line();
		assert!(!line.contains('\n'), "{}", case.case);
		assert_eq!(line, expected, "{}", case.case);
		checked += 1;
	}
	assert_eq!(checked, 35);
}

/// `identify`'s refusal of `code`, as the command loop words it.
fn identify_error(code: ErrorCode, message: &str) -> OutMessage {
	OutMessage::Reply {
		id: id(9),
		outcome: Err(ReplyError::new(code, message)),
	}
}

#[test]
fn an_identify_carries_its_path() {
	let message = parse_line(r#"{"t":"identify","id":4,"path":"C:\\r\\a b\\ü.txt"}"#).unwrap();
	let Request::Identify { path } = message.request else {
		panic!("not an identify");
	};
	assert_eq!(path.as_str(), "C:\\r\\a b\\ü.txt");
}

#[test]
fn an_identify_path_holds_32767_utf16_units_and_no_more() {
	assert!(ItemPath::parse(format!("C:\\{}", "a".repeat(32_764))).is_some());
	assert_eq!(ItemPath::parse(format!("C:\\{}", "a".repeat(32_765))), None);
	assert_eq!(ItemPath::parse(String::new()), None);
	assert_eq!(ItemPath::parse("C:\\a\0b".to_owned()), None);
}

#[test]
fn a_register_carries_its_fields() {
	let message =
		parse_line(r#"{"t":"register","id":1,"root":"C:\\r","userId":"42","dataDir":"C:\\d"}"#)
			.unwrap();
	let Request::Register(registration) = message.request else {
		panic!("not a register");
	};
	assert_eq!(registration.root.as_path(), Path::new("C:\\r"));
	assert_eq!(registration.data_dir, PathBuf::from("C:\\d"));
	assert_eq!(registration.user, UserId::from(42));
	assert_eq!(registration.branding, None);
}

#[test]
fn a_register_carries_the_branding_it_names() {
	let message = parse_line(
		r#"{"t":"register","id":1,"root":"C:\\r","userId":"42","dataDir":"C:\\d","displayName":"Filen (2)","iconPath":"C:\\F\\Filen.exe","iconIndex":-5}"#,
	)
	.unwrap();
	let Request::Register(registration) = message.request else {
		panic!("not a register");
	};
	let branding = registration.branding.unwrap();
	assert_eq!(branding.display_name.as_str(), "Filen (2)");
	assert_eq!(
		branding.icon.as_ref().map(|icon| icon.as_str()),
		Some("C:\\F\\Filen.exe")
	);
	assert_eq!(branding.icon_index, -5);
}

#[test]
fn a_register_carries_the_explorer_commands_it_names() {
	let message = parse_line(
		r#"{"t":"register","id":1,"root":"C:\\r","userId":"42","dataDir":"C:\\d","explorerCommands":{"appPath":"C:\\F\\Filen.exe","share":"Teilen"}}"#,
	)
	.unwrap();
	let Request::Register(registration) = message.request else {
		panic!("not a register");
	};
	let branding = registration.branding.unwrap();
	assert_eq!(branding.display_name.as_str(), "Filen");
	assert_eq!(
		branding.commands.map(|commands| *commands),
		Some(
			ExplorerCommands::parse("C:\\F\\Filen.exe", None, Some("Teilen"), None, None).unwrap()
		)
	);
}

#[test]
fn a_refused_app_path_names_its_rule_and_quotes_nothing() {
	let error = parse_line(
		r#"{"t":"register","id":3,"root":"C:\\r","userId":"42","dataDir":"C:\\d","explorerCommands":{"appPath":"C:\\secret%\\Filen.exe"}}"#,
	)
	.unwrap_err();
	assert_eq!(error.code, ErrorCode::InvalidMessage);
	assert_eq!(error.id, Some(id(3)));
	assert_eq!(
		error.message,
		"explorerCommands.appPath holds a control character, a %, a quote or a /"
	);
}

#[test]
fn a_refused_branding_names_its_rule_and_quotes_nothing() {
	let error = parse_line(
		r#"{"t":"register","id":3,"root":"C:\\r","userId":"42","dataDir":"C:\\d","displayName":"secret%"}"#,
	)
	.unwrap_err();
	assert_eq!(error.code, ErrorCode::InvalidMessage);
	assert_eq!(error.id, Some(id(3)));
	assert_eq!(error.message, "displayName holds a %");
}

#[test]
fn an_error_message_is_cut_to_what_the_parent_takes() {
	let long = "é".repeat(MAX_MESSAGE_CHARS + 10);
	let line = OutMessage::Reply {
		id: id(1),
		outcome: Err(ReplyError::new(ErrorCode::Io, long)),
	}
	.to_line();
	let value: Value = serde_json::from_str(&line).unwrap();
	let message = value["error"]["message"].as_str().unwrap();
	assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
	assert!(message.ends_with('…'));
	let exact = "a".repeat(MAX_MESSAGE_CHARS);
	let line = OutMessage::Fatal {
		code: FatalCode::Usage,
		message: exact.clone(),
	}
	.to_line();
	let value: Value = serde_json::from_str(&line).unwrap();
	assert_eq!(value["message"], exact.as_str());
}

#[test]
fn a_refused_session_quotes_none_of_it() {
	let line = r#"{"t":"setAuth","id":1,"userId":"42","json":"{\"providerEnabled\":true,\"sdkConfig\":{\"password\":\"s3cr3t-marker\"}}"}"#;
	let error = parse_line(line).unwrap_err();
	assert_eq!(error.code, ErrorCode::PayloadInvalid);
	assert!(!format!("{error:?}").contains("s3cr3t"));
}

#[test]
fn a_misplaced_secret_is_not_quoted_by_a_schema_error() {
	let line = r#"{"t":"clearAuth","id":1,"userId":5,"password":"s3cr3t-marker"}"#;
	let error = parse_line(line).unwrap_err();
	assert_eq!(error.code, ErrorCode::InvalidMessage);
	assert!(!format!("{error:?}").contains("s3cr3t"));
}

#[test]
fn a_user_id_names_its_key_account() {
	assert_eq!(UserId::from(42).key_account().as_str(), "42");
}

#[test]
fn a_kept_id_is_16_lowercase_hex_digits() {
	for id in [0, 0xab, u64::MAX] {
		let text = KeptId::from(id).to_string();
		assert_eq!(text.len(), 16);
		assert_eq!(KeptId::parse(&text), Some(KeptId::from(id)));
	}
	for text in [
		"",
		"ab",
		"00000000000000AB",
		"+000000000000000",
		"0000000000000000a",
	] {
		assert_eq!(KeptId::parse(text), None, "{text}");
	}
	assert_ne!(KeptId::random(), KeptId::random());
}
