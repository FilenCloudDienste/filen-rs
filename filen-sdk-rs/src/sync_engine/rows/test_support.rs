use super::{super::plan::confirm_agreed, Baseline, BaselineEntry};

impl Baseline {
	/// Confirm every row awaiting confirmation that `confirms` accepts, and hand back each row as
	/// it now stands: [`confirm`](Self::confirm) for a test that names the rows by a predicate.
	pub(in super::super) fn confirm_where(
		&mut self,
		mut confirms: impl FnMut(&BaselineEntry) -> bool,
	) -> Vec<BaselineEntry> {
		let rows: Vec<BaselineEntry> = self
			.unconfirmed()
			.filter(|row| confirms(row))
			.map(confirm_agreed)
			.collect();
		self.advance(&rows);
		rows
	}
}
