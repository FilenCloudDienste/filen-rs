//! Keep-both names for a top-level item whose destination turns out to hold the name it picked.

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	fs::name::{
		ValidatedName,
		keep_both::{NameShape, TakenNames},
	},
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
	shape: NameShape,
	subject: &'static str,
}

impl NameRetry {
	/// `subject` is what the error giving up calls the item: a copy says "copy", as it always
	/// has.
	pub(crate) fn new(shape: NameShape, subject: &'static str) -> Self {
		Self {
			taken: TakenNames::default(),
			attempts: 0,
			shape,
			subject,
		}
	}

	/// The next name after `taken_name`, or an error once [`TOP_LEVEL_NAME_ATTEMPTS`] names
	/// were tried.
	pub(crate) fn next(&mut self, taken_name: ValidatedName) -> Result<ValidatedName, Error> {
		self.attempts += 1;
		if self.attempts >= TOP_LEVEL_NAME_ATTEMPTS {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"could not find a free name for the {} at the destination",
					self.subject
				),
			));
		}
		self.taken.insert(taken_name.as_ref());
		Ok(self.taken.allocate(taken_name, self.shape)?)
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_copy_that_runs_out_of_names_says_so_as_it_always_has() {
		let mut retry = NameRetry::new(NameShape::File, "copy");
		let mut name = ValidatedName::try_from("a.txt").unwrap();
		for _ in 1..TOP_LEVEL_NAME_ATTEMPTS {
			name = retry.next(name).unwrap();
		}
		let error = retry.next(name).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState);
		assert_eq!(
			error.to_string(),
			"Error of kind InvalidState: context: could not find a free name for the copy at the \
			 destination"
		);
	}
}
