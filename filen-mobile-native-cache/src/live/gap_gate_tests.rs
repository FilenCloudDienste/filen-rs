use super::pass_owed;

/// The regression the durable debt exists for: a pass that failed leaves the watermark
/// behind, the next events carry it past the remote counter this cache ever compared
/// against, and from then on the counters alone say "caught up" forever. The debt must
/// outrank them — and, once a pass converges and clears it, stop forcing passes.
#[test]
fn an_owed_pass_runs_even_when_the_watermark_says_caught_up() {
	assert!(
		pass_owed(true, Some(50), 50),
		"a pass owed by a failed container (or apply) must run past a caught-up watermark"
	);
	assert!(
		pass_owed(true, Some(9000), 50),
		"a watermark advanced by later events must not swallow the debt"
	);
	assert!(
		!pass_owed(false, Some(50), 50),
		"nothing owed and nothing missed: no pass"
	);
	assert!(
		pass_owed(false, Some(49), 50),
		"a moved remote counter still runs a pass on its own"
	);
	assert!(
		pass_owed(false, None, 0),
		"a fresh cache has missed everything by definition"
	);
}
