use filen_mobile_native_cache::CacheError;

use super::*;

#[test]
fn every_cache_error_maps_to_its_literal_answer() {
	let table = [
		(
			CacheError::SQL("e".into()),
			Answer::Fail(Failure::Transient),
		),
		(
			CacheError::SDK("e".into()),
			Answer::Fail(Failure::Transient),
		),
		(
			CacheError::Conversion("e".into()),
			Answer::Fail(Failure::Transient),
		),
		(CacheError::IO("e".into()), Answer::Fail(Failure::Transient)),
		(
			CacheError::Remote("e".into()),
			Answer::Fail(Failure::Unreachable),
		),
		(
			CacheError::Image("e".into()),
			Answer::Fail(Failure::Transient),
		),
		(
			CacheError::Unauthenticated("e".into()),
			Answer::Fail(Failure::NotAuthenticated),
		),
		(
			CacheError::Disabled("e".into()),
			Answer::Fail(Failure::NotAuthenticated),
		),
		(
			CacheError::DoesNotExist("e".into()),
			Answer::Fail(Failure::NotFound),
		),
		(
			CacheError::Unsupported("e".into()),
			Answer::Fail(Failure::Unsupported),
		),
		(
			CacheError::NotADirectory("e".into()),
			Answer::Fail(Failure::NotFound),
		),
		(
			CacheError::FailedToDecrypt("e".into()),
			Answer::Fail(Failure::Undecryptable),
		),
		(
			CacheError::InvalidName("e".into()),
			Answer::Fail(Failure::InvalidRequest),
		),
		(
			CacheError::SyncAnchorExpired("e".into()),
			Answer::Fail(Failure::Internal),
		),
		(CacheError::Aborted("e".into()), Answer::Cancelled),
		(CacheError::ContentHashMismatch("e".into()), Answer::Damaged),
	];
	assert_eq!(table.len(), 16);
	for (error, answer) in table {
		assert_eq!(cache_answer(&error), answer, "{error:?}");
	}
}

#[test]
fn aborted_is_a_cancel_not_a_failure() {
	assert_eq!(
		cache_answer(&CacheError::Aborted("the call was aborted".into())),
		Answer::Cancelled
	);
}

#[test]
fn a_hash_mismatch_is_damaged_content() {
	assert_eq!(
		cache_answer(&CacheError::ContentHashMismatch("mismatch".into())),
		Answer::Damaged
	);
}
