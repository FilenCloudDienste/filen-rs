use super::*;

/// Every statement a pass reads with SEEKS the index it was written for, and none scans: at a
/// million rows a scan is the whole pair per question, which is the cost the resident tree
/// existed to avoid and the one this reader must not bring back. Asserted on the plan, naming
/// the index and the terms it seeks on — "SEARCH, and not SCAN" alone passes for a seek on
/// `pair_id` that then walks every row of the pair.
#[test]
fn every_statement_a_pass_reads_with_seeks_its_index() {
	let conn = super::super::super::baseline::test_rows_connection(1);
	let plan = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Vec<String> {
		conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
			.unwrap()
			.query_map(args, |row| row.get("detail"))
			.unwrap()
			.collect::<rusqlite::Result<_>>()
			.unwrap()
	};
	let primary = "PRIMARY KEY";
	let cases: [(String, Vec<&dyn rusqlite::ToSql>, &str, &str); 19] = [
		(
			dirs_sql(false, false),
			vec![&1_i64, &"a"],
			"baseline_dirs",
			"(pair_id=? AND rel_path>?)",
		),
		(
			dirs_sql(true, false),
			vec![&1_i64, &"a"],
			"baseline_dirs",
			"(pair_id=? AND rel_path>?)",
		),
		(
			dirs_sql(false, true),
			vec![&1_i64, &"a/", &"a0"],
			"baseline_dirs",
			"(pair_id=? AND rel_path>? AND rel_path<?)",
		),
		(
			dirs_sql(true, true),
			vec![&1_i64, &"a/", &"a0"],
			"baseline_dirs",
			"(pair_id=? AND rel_path>? AND rel_path<?)",
		),
		(
			UNCONFIRMED_PATHS.to_string(),
			vec![&1_i64],
			"baseline_unconfirmed",
			"(pair_id=?)",
		),
		(
			range_sql(true, false),
			vec![&1_i64, &"a"],
			primary,
			"(pair_id=? AND rel_path>?)",
		),
		(
			range_sql(false, true),
			vec![&1_i64, &"a/", &"a0"],
			primary,
			"(pair_id=? AND rel_path>? AND rel_path<?)",
		),
		(
			FOLDED_FROM.to_string(),
			vec![&1_i64, &"a"],
			"baseline_folded",
			"(pair_id=? AND folded_path>?)",
		),
		(
			FOLDED_UNDER.to_string(),
			vec![&1_i64, &"a/", &"", &"a0"],
			"baseline_folded",
			"(pair_id=? AND (folded_path,rel_path)>(?,?) AND folded_path<?)",
		),
		(
			UNCARRYABLE.to_string(),
			vec![&1_i64],
			"baseline_uncarryable",
			"(pair_id=?)",
		),
		(
			RULE_FILES.to_string(),
			vec![&1_i64],
			"baseline_rule_files",
			"(pair_id=?)",
		),
		(
			ANY_UNCONFIRMED.to_string(),
			vec![&1_i64],
			"baseline_unconfirmed",
			"(pair_id=?)",
		),
		(
			unconfirmed_sql(),
			vec![&1_i64, &"a"],
			"baseline_unconfirmed",
			"(pair_id=? AND rel_path>?)",
		),
		(
			unsynced_sql(false),
			vec![&1_i64],
			"baseline_state",
			"(pair_id=? AND state=?)",
		),
		(
			unsynced_sql(true),
			vec![&1_i64, &"a/", &"a0"],
			"baseline_state",
			"(pair_id=? AND state=? AND rel_path>? AND rel_path<?)",
		),
		(
			COUNTS.to_string(),
			vec![&1_i64],
			"baseline_counts",
			"INTEGER PRIMARY KEY (rowid=?)",
		),
		(
			// The one shape a caller never sends — `""` bounded — is still a seek.
			range_sql(false, false),
			vec![&1_i64, &""],
			primary,
			"(pair_id=? AND rel_path>?)",
		),
		(
			count_under_sql(true),
			vec![&1_i64, &"a/", &"a0"],
			primary,
			"(pair_id=? AND rel_path>? AND rel_path<?)",
		),
		(
			count_under_sql(false),
			vec![&1_i64, &""],
			primary,
			"(pair_id=? AND rel_path>?)",
		),
	];
	for (sql, args, index, seek) in &cases {
		let steps = plan(sql, args);
		assert!(
			steps.iter().any(|step| step.starts_with("SEARCH")
				&& step.contains(index)
				&& step.contains(seek)),
			"{sql}\nmust seek {index} on {seek}: {steps:?}"
		);
		assert!(
			!steps
				.iter()
				.any(|step| step.contains("SCAN") || step.contains("TEMP B-TREE")),
			"{sql}\nscans or sorts: {steps:?}"
		);
	}
}

/// No statement a pass reads with is RE-PREPARED when it is asked again with other values.
///
/// This build of SQLite has STAT4, under which a statement whose plan could depend on a bound
/// value — a bound `LIMIT` is one — is expired whenever that binding changes and re-prepared
/// inside its next step: a parse and a plan per page, invisible in every other test and most
/// of a whole pass's cost when it happened. Each read runs twice with different values, and
/// SQLite's own counter for the statement it ran has to still read zero.
#[test]
fn no_statement_a_pass_reads_with_is_prepared_again_per_call() {
	let rows = (0..40).map(|n| BaselineEntry {
		rel_path: format!("d{}/f{n:02}", n % 3),
		..crate::sync_engine::rows::tests::file_for_test(n)
	});
	let snapshot = standalone(rows);
	for (from, to) in [("d0", Some("d1")), ("d1/f", None)] {
		for inclusive in [false, true] {
			snapshot.page(from, inclusive, to, 3);
		}
	}
	snapshot.row("d0/f00");
	snapshot.row("d1/f01");
	snapshot.carryable_at("d0/f00");
	snapshot.carryable_at("d2/f02");
	snapshot.folded_at("d0");
	snapshot.folded_at("d1");
	snapshot.folded_under("d0", None, 2);
	snapshot.folded_under("d1", Some(("d1/f01", "d1/f01")), 2);
	snapshot.unsynced_under("");
	snapshot.unsynced_under("d0");
	snapshot.unsynced_under("d1");
	snapshot.count_under("d0");
	snapshot.count_under("d1");
	snapshot.unconfirmed_after("", 2);
	snapshot.unconfirmed_after("d1/f01", 2);
	let guard = snapshot.conn.lock().unwrap();
	let conn = guard.as_ref().unwrap();
	for sql in [
		&RANGE[0][0],
		&RANGE[0][1],
		&RANGE[1][0],
		&RANGE[1][1],
		FOLDED_FROM,
		FOLDED_UNDER,
		&UNSYNCED[1],
		&COUNT_UNDER[1],
		&UNCONFIRMED,
	] {
		let statement = conn.prepare_cached(sql).unwrap();
		assert_eq!(
			statement.get_status(rusqlite::StatementStatus::RePrepare),
			0,
			"re-prepared on a new binding: {sql}"
		);
		assert!(
			statement.get_status(rusqlite::StatementStatus::VmStep) > 0,
			"the read never ran this statement, so its counter says nothing: {sql}"
		);
	}
}

/// A point question answers what the row at that path is, and a folded one which rows fold to
/// that path, whatever order the questions come in — and questions walked in order read the
/// table a growing page at a time, where the same questions out of order read a row each. The
/// walk interleaves a path no row holds between every two rows, which is what a pass's decided
/// set does with the paths a move vacated; every third row has a twin by case, so one folded path
/// names two rows and a page can stop between them.
#[test]
fn questions_in_order_read_ahead_and_answer_as_one_row_each() {
	let row = |rel_path: String| BaselineEntry {
		rel_path,
		..crate::sync_engine::rows::tests::file_for_test(1)
	};
	let mut rows: Vec<BaselineEntry> = Vec::new();
	let mut folded: Vec<(String, Vec<String>)> = Vec::new();
	for n in 0..(MAX_PAGE.get() * 3) {
		let path = format!("d/f{n:05}");
		let mut at = vec![path.clone()];
		if n % 3 == 0 {
			let twin = format!("D/f{n:05}");
			rows.push(row(twin.clone()));
			at.insert(0, twin);
		}
		rows.push(row(path.clone()));
		folded.push((path.clone(), at));
		folded.push((format!("{path}x"), Vec::new()));
	}
	let mut asked: Vec<(String, Option<&BaselineEntry>)> = Vec::new();
	for row in rows.iter().filter(|row| row.rel_path.starts_with('d')) {
		asked.push((row.rel_path.clone(), Some(row)));
		asked.push((format!("{}x", row.rel_path), None));
	}
	let runs = |snapshot: &Snapshot, sql: &str| {
		let guard = snapshot.conn.lock().unwrap();
		let runs = guard
			.as_ref()
			.unwrap()
			.prepare_cached(sql)
			.unwrap()
			.get_status(rusqlite::StatementStatus::Run);
		usize::try_from(runs).unwrap()
	};
	// In order with and without the paths no row holds (a folded walk of a pass's decided set
	// has none between its rows), and backwards.
	for (reversed, absent) in [(false, true), (false, false), (true, true)] {
		let snapshot = standalone(rows.clone());
		let keep = |path: &str| absent || !path.ends_with('x');
		let mut order: Vec<&(String, Option<&BaselineEntry>)> =
			asked.iter().filter(|(path, _)| keep(path)).collect();
		let mut folded_order: Vec<&(String, Vec<String>)> =
			folded.iter().filter(|(key, _)| keep(key)).collect();
		if reversed {
			order.reverse();
			folded_order.reverse();
		}
		let context = format!("reversed: {reversed}, absent paths asked: {absent}");
		for (path, expected) in &order {
			assert_eq!(
				snapshot.row(path).as_ref(),
				*expected,
				"row({path:?}), {context}"
			);
			assert_eq!(
				snapshot.carryable_at(path),
				expected.map(BaselineEntry::carryable),
				"carryable_at({path:?}), {context}"
			);
			// Statements alone cannot see a backwards walk reading large pages: it misses every
			// one of them whatever their size.
			let held = snapshot
				.ahead
				.lock()
				.unwrap()
				.page()
				.map_or(0, |page| page.rows.len());
			assert!(
				!reversed || held <= 1,
				"a backwards walk read a {held}-row page for {path:?}, {context}"
			);
		}
		for (key, expected) in &folded_order {
			assert_eq!(
				&snapshot.folded_at(key),
				expected,
				"folded_at({key:?}), {context}"
			);
			// Twice the rows folded to the path at most: a page doubles until it reads past them.
			let held = snapshot
				.folded_ahead
				.lock()
				.unwrap()
				.page()
				.map_or(0, |page| page.rows.len());
			let bound = 2 * expected.len().max(1);
			assert!(
				!reversed || held <= bound,
				"a backwards walk read a {held}-row folded page for {key:?}, {context}"
			);
		}
		let (point, by_folded) = (runs(&snapshot, &RANGE[1][0]), runs(&snapshot, FOLDED_FROM));
		if reversed {
			assert!(
				point >= order.len() / 2 && by_folded >= folded_order.len() / 2,
				"out of order, every question about a row reads it on its own: {point} and \
					 {by_folded} read(s), {context}"
			);
		} else {
			assert!(
				point < 32 && by_folded < 32,
				"{} point and {} folded questions in order read {point} and {by_folded} \
					 page(s), {context}",
				order.len() * 2,
				folded_order.len()
			);
		}
	}
}

/// A folded question nothing read ahead for costs one statement, as the equality it replaced
/// did — also where a row holds the path, whose page must reach past it to answer.
#[test]
fn a_lone_folded_question_reads_one_page() {
	let row = |rel_path: &str| BaselineEntry {
		rel_path: rel_path.to_string(),
		..crate::sync_engine::rows::tests::file_for_test(1)
	};
	for (asked, expected) in [
		("b", vec!["b".to_string()]),
		("a", vec!["a".to_string()]),
		("c", Vec::new()),
	] {
		let snapshot = standalone(vec![row("a"), row("b")]);
		assert_eq!(snapshot.folded_at(asked), expected, "folded_at({asked:?})");
		let guard = snapshot.conn.lock().unwrap();
		let runs = guard
			.as_ref()
			.unwrap()
			.prepare_cached(FOLDED_FROM)
			.unwrap()
			.get_status(rusqlite::StatementStatus::Run);
		assert_eq!(runs, 1, "folded_at({asked:?}) read {runs} pages");
	}
}
