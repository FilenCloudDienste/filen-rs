//! The REPL's tab completion, against a remote directory in the test account.

use filen_macros::shared_test_runtime;

#[shared_test_runtime]
async fn completes_commands_and_remote_paths() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let root = resources.dir.clone();
	let root_path = root.meta.name().unwrap().to_owned();

	let test_completer = |input: &str, expected: &[&str]| {
		let mut expected = Vec::from(expected);
		expected.sort();
		let mut completions = filen_cli::complete_repl_input(input, client.clone(), &root_path);
		completions.sort(); // ignore order
		assert_eq!(
			completions, expected,
			"Unexpected completions for input '{}'",
			input
		)
	};

	test_utils::create_remote_file_structure_outline(
		client,
		root,
		&[
			"dir1/",
			"dir1/file_in_dir1.txt",
			"dir1/subdir1/file_in_subdir1.txt",
			"dir2/",
			"some_file.txt",
			"some_dir/",
			"a dir with spaces/",
			"a dir with spaces/file_in_dir_with_spaces.txt",
		],
	)
	.await
	.unwrap();

	// complete commands
	test_completer("c", &["cat", "cd", "cp"]);

	// basic completion of files and directories
	test_completer("ls d", &["ls dir1", "ls dir2"]);
	test_completer("ls dir1", &["ls dir1"]);

	// complete second arguments
	test_completer("cp some_file.txt some_d", &["cp some_file.txt some_dir"]);

	// differentiate between files and directories
	test_completer("cat some", &["cat some_file.txt", "cat some_dir"]); // directories should also be suggested for cat, because they might contain files that the user wants to cat
	test_completer("ls some", &["ls some_dir"]); // ls only accepts directories

	// completion in subdirectories
	test_completer("cat dir1/f", &["cat dir1/file_in_dir1.txt"]);
	test_completer("cat dir1/s", &["cat dir1/subdir1"]);
	test_completer(
		"cat dir1/subdir1/f",
		&["cat dir1/subdir1/file_in_subdir1.txt"],
	);

	// completion edge cases
	test_completer("ls dir1", &["ls dir1"]);
	// non-existing path
	test_completer("ls non_existing", &[]);
	// empty input
	test_completer("ls ", &[]);

	// completions that have spaces in them
	test_completer("ls a", &["ls \"a dir with spaces\""]);
	test_completer(
		"cat \"a dir with spaces/file\"",
		&["cat \"a dir with spaces/file_in_dir_with_spaces.txt\""],
	);
	test_completer(
		"cat \"a dir with spaces\"/f",
		&["cat \"a dir with spaces/file_in_dir_with_spaces.txt\""],
	);
	// a quote that is still open
	test_completer(
		"cat \"a dir with spaces/f",
		&["cat \"a dir with spaces/file_in_dir_with_spaces.txt\""],
	);
	test_completer(
		"cat a\\ dir\\ with\\ spaces/f",
		&["cat \"a dir with spaces/file_in_dir_with_spaces.txt\""],
	);

	// handle relative path parts like "." and ".."
	test_completer("ls .", &[]);
	test_completer(
		"ls ./",
		&[
			"ls ./dir1",
			"ls ./dir2",
			"ls ./some_dir",
			"ls \"./a dir with spaces\"",
		],
	);
}
