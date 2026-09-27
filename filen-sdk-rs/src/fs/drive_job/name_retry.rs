//! Keep-both names for a top-level item whose destination turns out to hold the name it picked.

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	fs::name::{ValidatedName, keep_both::TakenNames},
};

use super::backend::DriveBackend;

/// How many names a top-level item tries when the ones it picks turn out to be taken at the
/// destination (by an entry the listing could not name, or one created since the listing).
/// Bounded because the check runs while the drive lock is held.
pub(crate) const TOP_LEVEL_NAME_ATTEMPTS: usize = 8;

/// The keep-both names a top-level item moves through when the destination turns out to hold
/// the one it picked.
pub(crate) struct NameRetry {
	taken: TakenNames,
	attempts: usize,
	is_dir: bool,
}

impl NameRetry {
	pub(crate) fn new(is_dir: bool) -> Self {
		Self {
			taken: TakenNames::default(),
			attempts: 0,
			is_dir,
		}
	}

	/// The next name after `taken_name`, or an error once [`TOP_LEVEL_NAME_ATTEMPTS`] names
	/// were tried.
	pub(crate) fn next(&mut self, taken_name: ValidatedName) -> Result<ValidatedName, Error> {
		self.attempts += 1;
		if self.attempts >= TOP_LEVEL_NAME_ATTEMPTS {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"could not find a free name for the item at the destination",
			));
		}
		self.taken.insert(taken_name.as_ref());
		Ok(self.taken.allocate(taken_name, self.is_dir)?)
	}

	/// `name`, or the first following keep-both name the server reports free in `parent`.
	pub(crate) async fn free_name<B: DriveBackend>(
		&mut self,
		backend: &B,
		parent: Uuid,
		mut name: ValidatedName,
	) -> Result<ValidatedName, Error> {
		while backend.name_exists(parent, &name).await? {
			name = self.next(name)?;
		}
		Ok(name)
	}
}
