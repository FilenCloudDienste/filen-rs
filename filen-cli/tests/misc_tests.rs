//! Live tests of `public-link` and `list-public-links`. Each sets links up through the SDK, runs
//! the CLI on them, and checks the links through the SDK again.

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	connect::PasswordState,
	fs::{HasName as _, HasUUID as _},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;
use predicates::str::contains;
use test_utils::authenticated_cli_with_args;

/// Where the URLs of public links start.
const PUBLIC_LINK_BASE_URL: &str = "https://app.filen.io/#/";

async fn upload_file(client: &Client, parent: &RemoteDirectory, name: &str) -> RemoteFile {
	let file = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(file, b"linked").await.unwrap()
}

/// The absolute path of `name` in the test directory, which is a child of the root.
fn path_in(test_dir: &RemoteDirectory, name: &str) -> String {
	format!("/{}/{name}", test_dir.name().unwrap())
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
	String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

#[shared_test_runtime]
async fn public_link_show_and_a_run_without_a_terminal_change_nothing() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = upload_file(client, test_dir, "shown.txt").await;
	let mut file_link = client.public_link_file(&file).await.unwrap();
	file_link.set_expiration(PublicLinkExpiration::ThreeDays);
	file_link.set_password("secret".to_owned());
	file_link.set_downloadable(false);
	client.update_file_link(&file, &file_link).await.unwrap();
	let file_link = client.get_file_link_status(&file).await.unwrap().unwrap();

	let dir = client
		.create_dir(&test_dir.into(), "shown-dir")
		.await
		.unwrap();
	let mut dir_link = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();
	dir_link.set_expiration(PublicLinkExpiration::OneWeek);
	client.update_dir_link(&dir, &dir_link).await.unwrap();
	let dir_link = client.get_dir_link_rw(&dir).await.unwrap().unwrap();

	let file_path = path_in(test_dir, "shown.txt");
	let shown = authenticated_cli_with_args!("public-link", &file_path, "--show").success();
	let stdout = stdout_of(&shown);
	for expected in [
		"shown.txt".to_owned(),
		"Expiration 3 days".to_owned(),
		"Password Yes".to_owned(),
		"Download Enabled No".to_owned(),
		format!("Link UUID {}", file_link.uuid()),
		format!("URL {PUBLIC_LINK_BASE_URL}d/{}%23", file_link.uuid()),
	] {
		assert!(stdout.contains(&expected), "{expected:?} not in:\n{stdout}");
	}

	let dir_path = path_in(test_dir, "shown-dir");
	let shown = authenticated_cli_with_args!("public-link", &dir_path, "--show").success();
	let stdout = stdout_of(&shown);
	for expected in [
		"shown-dir".to_owned(),
		"Expiration 1 week".to_owned(),
		"Password No".to_owned(),
		"Download Enabled Yes".to_owned(),
		format!("URL {PUBLIC_LINK_BASE_URL}f/{}%23", dir_link.uuid()),
	] {
		assert!(stdout.contains(&expected), "{expected:?} not in:\n{stdout}");
	}

	// without a terminal on stdin (the test's stdin is closed), no options only shows the link
	authenticated_cli_with_args!("public-link", &file_path)
		.success()
		.stdout(contains("Expiration 3 days"));
	authenticated_cli_with_args!("public-link", &dir_path)
		.success()
		.stdout(contains("Expiration 1 week"));

	let file_link_after = client.get_file_link_status(&file).await.unwrap().unwrap();
	assert_eq!(file_link_after, file_link);
	assert!(matches!(
		file_link_after.password(),
		PasswordState::Hashed(_)
	));
	assert_eq!(
		client.get_dir_link_rw(&dir).await.unwrap().unwrap(),
		dir_link
	);

	// an item without a link is shown to have none, and gets none
	let unlinked = upload_file(client, test_dir, "unlinked.txt").await;
	authenticated_cli_with_args!("public-link", &path_in(test_dir, "unlinked.txt"), "--show")
		.success()
		.stdout(contains(format!(
			"No public link for {}.",
			path_in(test_dir, "unlinked.txt")
		)));
	assert_eq!(client.get_file_link_status(&unlinked).await.unwrap(), None);
	// and without a terminal, no options does not make it public unasked
	authenticated_cli_with_args!("public-link", &path_in(test_dir, "unlinked.txt"))
		.code(2)
		.stderr(contains("--expires never"));
	assert_eq!(client.get_file_link_status(&unlinked).await.unwrap(), None);

	client
		.remove_file_link(&file, file_link_after)
		.await
		.unwrap();
	client.remove_dir_link(&dir).await.unwrap();
}

#[shared_test_runtime]
async fn list_public_links_shows_the_path_and_url_of_each_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = upload_file(client, test_dir, "listed.txt").await;
	let file_link = client.public_link_file(&file).await.unwrap();
	let dir = client
		.create_dir(&test_dir.into(), "listed-dir")
		.await
		.unwrap();
	let dir_link = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();
	let file_path = path_in(test_dir, "listed.txt");
	let dir_path = path_in(test_dir, "listed-dir");
	let file_url = format!("{PUBLIC_LINK_BASE_URL}d/{}%23", file_link.uuid());
	let dir_url = format!("{PUBLIC_LINK_BASE_URL}f/{}%23", dir_link.uuid());

	let listed = authenticated_cli_with_args!("list-public-links").success();
	let stdout = stdout_of(&listed);
	// the account is shared with other tests, so only this test's lines are checked
	for (path, url) in [(&file_path, &file_url), (&dir_path, &dir_url)] {
		let line = stdout
			.lines()
			.map(|line| console::strip_ansi_codes(line).into_owned())
			.find(|line| line.starts_with(&format!("{path} ")))
			.unwrap_or_else(|| panic!("{path} not listed in:\n{stdout}"));
		assert!(line.contains(url.as_str()), "{url} not in {line:?}");
	}

	let listed = authenticated_cli_with_args!("list-public-links", "--json").success();
	let json: serde_json::Value = serde_json::from_str(&stdout_of(&listed)).unwrap();
	let mut ours = json
		.as_array()
		.unwrap()
		.iter()
		.filter(|link| {
			link["path"]
				.as_str()
				.unwrap()
				.starts_with(&path_in(test_dir, ""))
		})
		.collect::<Vec<_>>();
	ours.sort_by_key(|link| link["path"].as_str().unwrap().to_owned());
	let [dir_entry, file_entry] = ours.as_slice() else {
		panic!("expected this test's two links in {json:#}");
	};
	for (entry, path, kind, link_uuid, url) in [
		(
			file_entry,
			&file_path,
			"file",
			file_link.uuid().to_string(),
			&file_url,
		),
		(
			dir_entry,
			&dir_path,
			"directory",
			dir_link.uuid().to_string(),
			&dir_url,
		),
	] {
		assert_eq!(entry["path"], path.as_str());
		assert_eq!(entry["type"], kind);
		assert_eq!(entry["linkUuid"], link_uuid.as_str());
		assert!(
			entry["url"].as_str().unwrap().starts_with(url.as_str()),
			"{entry}"
		);
	}

	client.remove_file_link(&file, file_link).await.unwrap();
	client.remove_dir_link(&dir).await.unwrap();
}

#[shared_test_runtime]
async fn public_link_options_set_and_delete_a_link_without_prompting() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = upload_file(client, test_dir, "flagged.txt").await;
	let path = path_in(test_dir, "flagged.txt");

	authenticated_cli_with_args!(
		"public-link",
		&path,
		"--expires",
		"1d",
		"--no-download",
		"--password",
		"x"
	)
	.success()
	.stdout(contains("Public link saved."));
	let link = client.get_file_link_status(&file).await.unwrap().unwrap();
	assert_eq!(link.expiration(), PublicLinkExpiration::OneDay);
	assert!(!link.downloadable());
	assert!(matches!(link.password(), PasswordState::Hashed(_)));

	authenticated_cli_with_args!("public-link", &path, "--no-password").success();
	let unprotected = client.get_file_link_status(&file).await.unwrap().unwrap();
	assert_eq!(unprotected.uuid(), link.uuid());
	assert_eq!(unprotected.password(), &PasswordState::None);
	assert_eq!(unprotected.expiration(), PublicLinkExpiration::OneDay);
	assert!(!unprotected.downloadable());

	// without --yes and without a terminal, the deletion cannot be confirmed
	authenticated_cli_with_args!("public-link", &path, "--delete")
		.code(2)
		.stderr(contains("Pass --yes to answer yes."));
	assert!(client.get_file_link_status(&file).await.unwrap().is_some());

	authenticated_cli_with_args!("public-link", &path, "--delete", "--yes")
		.success()
		.stdout(contains("Public link deleted."));
	assert_eq!(client.get_file_link_status(&file).await.unwrap(), None);

	authenticated_cli_with_args!("public-link", &path, "--delete", "--yes")
		.code(1)
		.stderr(contains(format!("No public link for {path}")));

	// a setting option on a directory without a link creates one with that setting
	let dir = client
		.create_dir(&test_dir.into(), "flagged-dir")
		.await
		.unwrap();
	authenticated_cli_with_args!(
		"public-link",
		&path_in(test_dir, "flagged-dir"),
		"--expires",
		"7d"
	)
	.success();
	let dir_link = client.get_dir_link_rw(&dir).await.unwrap().unwrap();
	assert_eq!(dir_link.expiration(), PublicLinkExpiration::OneWeek);
	assert_eq!(dir_link.password(), &PasswordState::None);
	assert!(dir_link.download_enabled());

	client.remove_dir_link(&dir).await.unwrap();
}
