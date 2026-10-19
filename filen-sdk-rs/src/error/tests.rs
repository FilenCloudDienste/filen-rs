use std::io;

use super::*;

fn make_io_error() -> io::Error {
	io::Error::other("leaf io error")
}

/// The one foreseeable conversion failure — an anonymous file fed to a
/// drive op — must surface with its own switchable kind, not the blanket
/// `Conversion` bucket (same rule that gave `StaleState` its kind).
#[test]
fn missing_stable_uuid_gets_its_own_kind() {
	let err = Error::from(crate::crypto::error::ConversionError::MissingStableUuid);
	assert_eq!(err.kind(), ErrorKind::MissingStableUuid);
	let other = Error::from(crate::crypto::error::ConversionError::from(
		filen_types::error::ConversionError::ParentUuidError(String::new()),
	));
	assert_eq!(other.kind(), ErrorKind::Conversion);
}

#[test]
fn optional_returns_some_for_ok() {
	let r: Result<i32, Error> = Ok(42);
	assert_eq!(r.optional().unwrap(), Some(42));
}

#[test]
fn optional_returns_none_for_file_not_found() {
	let err = Error::custom(ErrorKind::FileNotFound, "missing file");
	let r: Result<i32, Error> = Err(err);
	assert_eq!(r.optional().unwrap(), None);
}

#[test]
fn optional_returns_none_for_folder_not_found() {
	let err = Error::custom(ErrorKind::FolderNotFound, "missing folder");
	let r: Result<i32, Error> = Err(err);
	assert_eq!(r.optional().unwrap(), None);
}

#[test]
fn optional_propagates_other_error_kinds() {
	let err = Error::custom(ErrorKind::Server, "boom");
	let r: Result<i32, Error> = Err(err);
	let propagated = r.optional().unwrap_err();
	assert_eq!(propagated.kind(), ErrorKind::Server);
}

#[test]
fn optional_converts_foreign_error_via_from() {
	// io::Error has a From impl for Error mapping to ErrorKind::IO,
	// which is not a "not found" kind even when the io kind is NotFound.
	let r: Result<i32, io::Error> =
		Err(io::Error::new(io::ErrorKind::NotFound, "not the SDK kind"));
	let propagated = r.optional().unwrap_err();
	assert_eq!(propagated.kind(), ErrorKind::IO);
}

#[test]
fn sdk_error_keeps_its_kind_through_an_io_error() {
	let io = io::Error::other(Error::custom(ErrorKind::MaxStorageReached, "quota"));
	assert_eq!(Error::from(io).kind(), ErrorKind::MaxStorageReached);
}

#[test]
fn sdk_error_keeps_its_kind_through_nested_io_errors() {
	let inner = io::Error::other(Error::custom(ErrorKind::Reqwest, "network"));
	let outer = io::Error::other(inner);
	assert_eq!(Error::from(outer).kind(), ErrorKind::Reqwest);
}

#[test]
fn plain_io_error_maps_to_io_kind() {
	let err = Error::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
	assert_eq!(err.kind(), ErrorKind::IO);
	assert_eq!(
		err.downcast_ref::<io::Error>().map(io::Error::kind),
		Some(io::ErrorKind::PermissionDenied)
	);
	// A non-SDK payload stays wrapped, with its io kind intact.
	let wrapped = Error::from(io::Error::other(std::fmt::Error));
	assert_eq!(wrapped.kind(), ErrorKind::IO);
	assert!(wrapped.downcast_ref::<io::Error>().is_some());
}

#[test]
fn downcast_finds_direct_inner_and_returns_context() {
	let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("ctx_outer"));
	let (inner, ctxs) = err.downcast::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
	assert_eq!(ctxs, vec![Cow::Borrowed("ctx_outer")]);
}

#[test]
fn downcast_finds_direct_inner_with_no_context() {
	let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
	let (inner, ctxs) = err.downcast::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
	assert!(ctxs.is_empty());
}

#[test]
fn downcast_returns_self_when_type_does_not_match() {
	let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("ctx"));
	let returned = err.downcast::<std::fmt::Error>().unwrap_err();
	assert_eq!(returned.kind(), ErrorKind::IO);
	// The original inner error must still be accessible.
	assert!(returned.downcast_ref::<io::Error>().is_some());
}

#[test]
fn downcast_returns_self_when_no_inner() {
	let err = Error::custom(ErrorKind::Server, "no inner");
	let returned = err.downcast::<io::Error>().unwrap_err();
	assert_eq!(returned.kind(), ErrorKind::Server);
}

#[test]
fn downcast_unwraps_nested_filen_sdk_errors() {
	let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("leaf_ctx"));
	let middle = Error::custom_with_source(ErrorKind::IO, leaf, Some("middle_ctx"));
	let outer = Error::custom_with_source(ErrorKind::IO, middle, Some("outer_ctx"));

	let (inner, ctxs) = outer.downcast::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
	// Context order: outermost first, innermost last.
	assert_eq!(
		ctxs,
		vec![
			Cow::Borrowed("outer_ctx"),
			Cow::Borrowed("middle_ctx"),
			Cow::Borrowed("leaf_ctx"),
		]
	);
}

#[test]
fn downcast_skips_missing_intermediate_contexts() {
	let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("leaf_ctx"));
	let middle = Error::custom_with_source(ErrorKind::IO, leaf, None::<&'static str>);
	let outer = Error::custom_with_source(ErrorKind::IO, middle, Some("outer_ctx"));

	let (_, ctxs) = outer.downcast::<io::Error>().unwrap();
	// `None` context is collected as an empty Vec entry — verify it does not leak in.
	assert_eq!(
		ctxs,
		vec![Cow::Borrowed("outer_ctx"), Cow::Borrowed("leaf_ctx")]
	);
}

#[test]
fn downcast_returns_self_when_nested_does_not_contain_type() {
	let inner = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("inner_ctx"));
	let outer = Error::custom_with_source(ErrorKind::IO, inner, Some("outer_ctx"));

	// Type isn't io::Error in any layer? It is — let's pick a different type.
	let returned = outer.downcast::<std::fmt::Error>().unwrap_err();
	assert_eq!(returned.kind(), ErrorKind::IO);
	// The nested chain must remain intact and reachable via downcast_ref.
	assert!(returned.downcast_ref::<io::Error>().is_some());
}

#[test]
fn downcast_ref_finds_direct_inner() {
	let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
	let inner = err.downcast_ref::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
}

#[test]
fn downcast_ref_finds_nested_inner() {
	let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
	let middle = Error::custom_with_source(ErrorKind::IO, leaf, None::<&'static str>);
	let outer = Error::custom_with_source(ErrorKind::Server, middle, None::<&'static str>);

	let inner = outer.downcast_ref::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
}

#[test]
fn downcast_ref_returns_none_when_type_missing() {
	let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
	assert!(err.downcast_ref::<std::fmt::Error>().is_none());
}

#[test]
fn downcast_ref_returns_none_when_no_inner() {
	let err = Error::custom(ErrorKind::Server, "nothing");
	assert!(err.downcast_ref::<io::Error>().is_none());
}

#[test]
fn downcast_ref_looks_through_a_shared_error() {
	let shared = Arc::new(Error::custom_with_source(
		ErrorKind::IO,
		make_io_error(),
		None::<&'static str>,
	));
	let outer = Error::custom_with_source(ErrorKind::IO, shared, None::<&'static str>);
	let inner = outer.downcast_ref::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
}

#[test]
fn downcast_takes_apart_a_shared_error_it_alone_holds() {
	let shared = Arc::new(Error::custom_with_source(
		ErrorKind::IO,
		make_io_error(),
		Some("leaf_ctx"),
	));
	let outer = Error::custom_with_source(ErrorKind::IO, shared, Some("outer_ctx"));
	let (inner, ctxs) = outer.downcast::<io::Error>().unwrap();
	assert_eq!(inner.to_string(), "leaf io error");
	assert_eq!(
		ctxs,
		vec![Cow::Borrowed("outer_ctx"), Cow::Borrowed("leaf_ctx")]
	);
}

#[test]
fn downcast_returns_self_while_a_shared_error_has_other_owners() {
	let shared = Arc::new(Error::custom_with_source(
		ErrorKind::IO,
		make_io_error(),
		None::<&'static str>,
	));
	let outer = Error::custom_with_source(ErrorKind::IO, Arc::clone(&shared), None::<&'static str>);
	let returned = outer.downcast::<io::Error>().unwrap_err();
	assert!(returned.downcast_ref::<io::Error>().is_some());
	drop(shared);
	assert!(returned.downcast::<io::Error>().is_ok());
}

#[test]
fn downcast_puts_a_shared_error_back_as_shared_when_the_type_is_missing() {
	let shared = Arc::new(Error::custom_with_source(
		ErrorKind::IO,
		make_io_error(),
		None::<&'static str>,
	));
	let outer = Error::custom_with_source(ErrorKind::IO, shared, None::<&'static str>);
	let returned = outer.downcast::<std::fmt::Error>().unwrap_err();
	let source = returned.inner.as_deref().unwrap();
	assert!(source.downcast_ref::<Arc<Error>>().is_some());
	assert!(returned.downcast_ref::<io::Error>().is_some());
}

#[test]
fn an_unshared_error_still_held_elsewhere_reads_as_the_shared_one() {
	let shared = Arc::new(
		Error::from(filen_types::error::ResponseError::ApiError {
			message: Some("Max storage reached".into()),
			code: Some("max_storage_reached".into()),
		})
		.with_context("upload"),
	);
	let unshared = Error::unshared(Arc::clone(&shared));
	assert!(
		unshared.downcast_ref::<Arc<Error>>().is_some(),
		"a stand-in"
	);
	assert_eq!(unshared.kind(), ErrorKind::MaxStorageReached);
	assert_eq!(unshared.message(), shared.message());
	assert_eq!(unshared.inner_message(), shared.inner_message());
	assert_eq!(
		unshared.server_code().as_deref(),
		Some("max_storage_reached")
	);
	assert_eq!(
		unshared.server_message().as_deref(),
		Some("Max storage reached")
	);
}

#[test]
fn an_error_wrapping_a_shared_one_with_its_own_context_reads_as_itself() {
	let shared = Arc::new(Error::custom(ErrorKind::IO, "leaf"));
	let outer = Error::custom_with_source(ErrorKind::IO, Arc::clone(&shared), Some("outer"));
	assert_eq!(
		outer.message(),
		"Error of kind IO: context: outer, error: Error of kind IO: context: leaf"
	);
}

#[test]
fn response_error_file_not_found_maps_to_file_not_found_kind() {
	let response_err = filen_types::error::ResponseError::ApiError {
		message: Some("File not found".into()),
		code: Some("file_not_found".into()),
	};
	let err: Error = response_err.into();
	assert_eq!(err.kind(), ErrorKind::FileNotFound);
}

#[test]
fn response_error_folder_not_found_still_maps_to_folder_not_found_kind() {
	// Sanity check that adding the new arm did not break the existing folder mapping.
	let response_err = filen_types::error::ResponseError::ApiError {
		message: None,
		code: Some("folder_not_found".into()),
	};
	let err: Error = response_err.into();
	assert_eq!(err.kind(), ErrorKind::FolderNotFound);
}

#[test]
fn response_error_unknown_code_falls_through_to_server() {
	let response_err = filen_types::error::ResponseError::ApiError {
		message: None,
		code: Some("some_brand_new_code".into()),
	};
	let err: Error = response_err.into();
	assert_eq!(err.kind(), ErrorKind::Server);
}

#[test]
fn server_message_and_code_expose_the_api_error() {
	let response_err = filen_types::error::ResponseError::ApiError {
		message: Some("Invalid API key".into()),
		code: Some("api_key_not_found".into()),
	};
	let err: Error = response_err.into();
	assert_eq!(err.server_message().as_deref(), Some("Invalid API key"));
	assert_eq!(err.server_code().as_deref(), Some("api_key_not_found"));
}

#[test]
fn server_message_and_code_are_none_for_non_server_errors() {
	let err = Error::custom_with_source(
		ErrorKind::IO,
		io::Error::other("boom"),
		None::<&'static str>,
	);
	assert!(err.server_message().is_none());
	assert!(err.server_code().is_none());
}

#[test]
fn inner_message_returns_underlying_message_without_wrapper() {
	let err = Error::custom_with_source(
		ErrorKind::IO,
		io::Error::other("boom"),
		None::<&'static str>,
	);
	assert_eq!(err.inner_message().as_deref(), Some("boom"));
	// Distinct from the developer-oriented `message()` which wraps the kind.
	assert!(err.message().starts_with("Error of kind"));
}

#[test]
fn inner_message_is_none_when_there_is_no_inner() {
	let err = Error::custom(ErrorKind::Server, "no inner here");
	assert!(err.inner_message().is_none());
}
