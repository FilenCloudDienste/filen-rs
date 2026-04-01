use std::{
	borrow::Cow,
	fs::File,
	path::{Path, PathBuf},
	time::{Duration, SystemTime},
};

use chrono::{SubsecRound, Utc};
use filen_macros::shared_test_runtime;

use filen_sdk_rs::{
	Error, ErrorKind,
	connect::{DirPublicLink, PublicLinkSharedClientExt},
	crypto::shared::generate_random_base64_values,
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID,
		categories::{
			DirType, NonRootFileType, NonRootItemType, Normal,
			fs::{CategoryFS, CategoryFSExt, ObjectOrRemainingPath},
		},
		dir::{
			RemoteDirectory,
			meta::{DirectoryMeta, DirectoryMetaChanges},
			traits::HasDirMeta,
		},
		file::{RemoteFile, meta::FileMeta, traits::HasFileMeta},
		name::EntryNameError,
	},
	io::{DirUploadCallback, FilenMetaExt},
};
use filen_types::fs::ParentUuid;
use futures::StreamExt;
use tokio::time;

#[shared_test_runtime]
async fn create_list_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "test_dir")
		.await
		.unwrap();
	assert_eq!(dir.name().unwrap(), "test_dir");

	let (dirs, _) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}

	client.trash_dir(&mut dir).await.unwrap();

	let (trashed_dirs, _) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let found = trashed_dirs
		.into_iter()
		.find(|d| d.uuid() == dir.uuid())
		.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(*test_dir.uuid()));

	let found_dir = client.get_dir(*dir.uuid()).await.unwrap();
	assert_eq!(dir, found_dir);
}

#[derive(Default)]
struct DebugDirUploadCallback {}

impl DirUploadCallback for DebugDirUploadCallback {
	fn on_scan_progress(&self, _known_dirs: u64, _known_files: u64, _known_bytes: u64) {}

	fn on_scan_errors(&self, _error: Vec<Error>) {}

	fn on_scan_complete(&self, _total_dirs: u64, _total_files: u64, _total_bytes: u64) {}

	fn on_upload_update(
		&self,
		_uploaded_dirs: Vec<filen_sdk_rs::fs::dir::RemoteDirectory>,
		_uploaded_files: Vec<RemoteFile>,
		_uploaded_bytes: u64,
	) {
	}

	fn on_upload_errors(&self, _errors: Vec<(PathBuf, filen_sdk_rs::Error)>) {}
}

struct TestFile {
	relative_path: &'static str,
	content: &'static str,
	modified_days_ago: u64,
}

fn expected_contents() -> Vec<TestFile> {
	vec![
		TestFile {
			relative_path: "readme.txt",
			content: "This is the root readme file.\n",
			modified_days_ago: 0,
		},
		TestFile {
			relative_path: "config.json",
			content: r#"{"version": 1, "name": "test"}"#,
			modified_days_ago: 7,
		},
		TestFile {
			relative_path: "documents/report.txt",
			content: "Monthly report content here.\n",
			modified_days_ago: 30,
		},
		TestFile {
			relative_path: "documents/notes.md",
			content: "# Notes\n\n- Item 1\n- Item 2\n",
			modified_days_ago: 14,
		},
		TestFile {
			relative_path: "images/metadata.json",
			content: r#"{"images": []}"#,
			modified_days_ago: 3,
		},
		TestFile {
			relative_path: "nested/deeply/buried/secret.txt",
			content: "You found the deeply nested file!\n",
			modified_days_ago: 365,
		},
		TestFile {
			relative_path: "empty.txt",
			content: "",
			modified_days_ago: 1,
		},
	]
}

async fn create_dummy_folder(base_path: &Path) -> std::io::Result<()> {
	tokio::fs::create_dir_all(base_path).await?;

	let subdirs = [
		"documents",
		"images",
		"nested/deeply/buried",
		"extra/nested",
	];
	for subdir in subdirs {
		tokio::fs::create_dir_all(base_path.join(subdir)).await?;
	}

	for test_file in expected_contents() {
		let full_path = base_path.join(test_file.relative_path);
		tokio::fs::write(&full_path, test_file.content).await?;

		let modified_time =
			SystemTime::now() - Duration::from_secs(test_file.modified_days_ago * 24 * 60 * 60);

		let file = File::options().write(true).open(&full_path)?;
		file.set_modified(modified_time)?;
	}

	Ok(())
}

async fn check_remote_matches_local(
	client: &filen_sdk_rs::auth::Client,
	remote_dir: &RemoteDirectory,
	local_path: &Path,
) -> std::io::Result<()> {
	let mut walk = walkdir::WalkDir::new(local_path).into_iter();
	let _ = walk.next(); // skip root

	futures::stream::iter(walk)
		.filter_map(|entry| async move {
			let entry = match entry {
				Ok(e) => e,
				Err(e) => {
					log::error!("Failed to read directory entry: {:?}", e);
					return None;
				}
			};
			let path = entry.path().to_path_buf();
			let relative_path = path.strip_prefix(local_path).unwrap().to_path_buf();
			let meta = tokio::fs::metadata(&path).await.ok()?;
			Some((path, relative_path, meta))
		})
		.map(|(path, relative_path, meta)| async move {
			let (_, item) = <Normal as CategoryFSExt>::get_items_in_path_starting_at(
				client,
				relative_path.to_str().unwrap(),
				DirType::Dir(Cow::Borrowed(remote_dir)),
				(),
			)
			.await
			.unwrap();

			let ObjectOrRemainingPath::Object(object) = item else {
				panic!("Uploaded item not found: {:?}", relative_path);
			};

			if meta.is_dir() {
				match object {
					NonRootFileType::Dir(dir) => {
						let dir = dir.as_ref();
						assert_eq!(
							dir.name().unwrap(),
							path.file_name().unwrap().to_str().unwrap(),
							"Directory name mismatch for path: {:?}",
							path
						);
						let mtime = FilenMetaExt::created(&meta);
						let DirectoryMeta::Decoded(decrypted_meta) = dir.get_meta() else {
							panic!("Directory meta not found for path: {:?}", path);
						};
						assert_eq!(
							decrypted_meta.created,
							Some(mtime),
							"Directory created time mismatch for path: {:?}",
							path
						);
					}
					_ => panic!("Expected directory for path: {:?}", path),
				}
			} else if meta.is_file() {
				match object {
					NonRootFileType::File(file) => {
						let file = file.as_ref();
						assert_eq!(
							file.name().unwrap(),
							path.file_name().unwrap().to_str().unwrap(),
							"File name mismatch for path: {:?}",
							path
						);
						let FileMeta::Decoded(decrypted_meta) = file.get_meta() else {
							panic!("File meta not found for path: {:?}", path);
						};
						let mtime = FilenMetaExt::modified(&meta);
						assert_eq!(
							decrypted_meta.last_modified(),
							mtime,
							"File modified time mismatch for path: {:?}",
							path
						);
						let ctime = FilenMetaExt::created(&meta);
						assert_eq!(
							decrypted_meta.created,
							Some(ctime),
							"File created time mismatch for path: {:?}",
							path
						);
					}
					_ => panic!("Expected file for path: {:?}", relative_path),
				}
			} else {
				panic!("Unknown file type for path: {:?}", relative_path);
			}
		})
		.buffer_unordered(20) // Adjust concurrency limit as needed
		.collect::<Vec<_>>()
		.await;

	Ok(())
}

#[shared_test_runtime]
async fn test_recursive_upload() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let temp_dir = std::env::temp_dir().join(format!("test_upload_{}", std::process::id()));

	// Clean up if it exists from a previous run
	let _ = tokio::fs::remove_dir_all(&temp_dir).await;

	create_dummy_folder(&temp_dir).await.unwrap();
	client
		.clone()
		.upload_dir_recursively(
			temp_dir.clone(),
			&DebugDirUploadCallback::default(),
			test_dir,
		)
		.await
		.unwrap();

	check_remote_matches_local(&client, test_dir, &temp_dir)
		.await
		.unwrap();

	// Clean up
	let _ = tokio::fs::remove_dir_all(&temp_dir).await;
}

#[shared_test_runtime]
async fn find_at_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(&test_dir.into(), "a").await.unwrap();
	let dir_b = client.create_dir(&(&dir_a).into(), "b").await.unwrap();
	let dir_c = client.create_dir(&(&dir_b).into(), "c").await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/a/b/c", test_dir.name().unwrap()))
			.await
			.unwrap(),
		Some(NonRootFileType::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/a/bc", test_dir.name().unwrap()))
			.await
			.unwrap(),
		None
	);

	let path = format!("{}/a/b/c", test_dir.name().unwrap());

	let items = client.get_items_in_path(&path).await.unwrap();

	assert_eq!(items.0.len(), 4);
	assert!(items.0.contains(&DirType::Dir(Cow::Borrowed(&dir_a))));
	assert!(items.0.contains(&DirType::Dir(Cow::Borrowed(&dir_b))));
	assert!(!items.0.contains(&DirType::Dir(Cow::Borrowed(&dir_c))));
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(NonRootFileType::Dir(dir)) if *dir == dir_c
	));

	let items = <Normal as CategoryFSExt>::get_items_in_path_starting_at(
		&**client,
		"b/c",
		DirType::Dir(Cow::Borrowed(&dir_a)),
		(),
	)
	.await
	.unwrap();
	assert_eq!(items.0.len(), 2);
	assert!(items.0.contains(&DirType::Dir(Cow::Borrowed(&dir_a))));
	assert!(items.0.contains(&DirType::Dir(Cow::Borrowed(&dir_b))));
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(NonRootFileType::Dir(dir)) if *dir == dir_c
	));

	let resp = <Normal as CategoryFSExt>::get_items_in_path_starting_at(
		&**client,
		"c/d/e",
		DirType::Dir(Cow::Borrowed(&dir_b)),
		(),
	)
	.await
	.unwrap();
	// Expecting None because "c/d/e" does not exist in the path
	// and the last item in dirs be the directory "c"
	match resp {
		(mut dirs, ObjectOrRemainingPath::RemainingPath(path)) => match dirs.pop() {
			Some(DirType::Dir(dir)) if *dir == dir_c => {
				assert_eq!(path, "d/e");
			}
			other => panic!("Expected last directory to be 'c', but got: {other:?}"),
		},
		other => panic!("Expected dir_c, but got: {other:?}"),
	}
}

#[shared_test_runtime]
async fn find_or_create() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let path = format!("{}/a/b/c", test_dir.name().unwrap());
	let nested_dir = client.find_or_create_dir(&path).await.unwrap();

	assert_eq!(
		Some(Into::<NonRootFileType<'_, Normal>>::into(
			nested_dir.clone()
		)),
		client.find_item_at_path(&path).await.unwrap()
	);

	let nested_dir = client
		.find_or_create_dir_starting_at(nested_dir, "d/e")
		.await
		.unwrap();
	assert_eq!(
		Some(Into::<NonRootFileType<'_, Normal>>::into(
			nested_dir.clone()
		)),
		client
			.find_item_at_path(&format!("{path}/d/e"))
			.await
			.unwrap()
	);
}

#[shared_test_runtime]
async fn list_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(&test_dir.into(), "a").await.unwrap();
	let dir_b = client.create_dir(&(&dir_a).into(), "b").await.unwrap();
	let dir_c = client.create_dir(&(&dir_b).into(), "c").await.unwrap();

	let (dirs, _) = client
		.list_dir_recursive(&test_dir.into(), &|downloaded, total| {
			log::trace!(
				"List dir recursive progress: downloaded {} / {:?}",
				downloaded,
				total
			);
		})
		.await
		.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));

	let (dirs, _) = Normal::list_dir_recursive_with_paths(
		resources.client.clone(),
		test_dir.into(),
		Some(&|_: u64, _: Option<u64>| {}),
		&mut |errors| {
			panic!("received unexpected errors: {:?}", errors);
		},
		(),
	)
	.await
	.unwrap();

	assert!(dirs.contains(&(dir_a, "a".to_string())));
	assert!(dirs.contains(&(dir_b, "a/b".to_string())));
	assert!(dirs.contains(&(dir_c, "a/b/c".to_string())));
}

#[shared_test_runtime]
async fn exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(
		client
			.dir_exists(&test_dir.into(), "a")
			.await
			.unwrap()
			.is_none()
	);

	let mut dir_a = client.create_dir(&test_dir.into(), "a").await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		client
			.dir_exists(&test_dir.into(), "a")
			.await
			.unwrap()
			.as_ref()
	);

	client.trash_dir(&mut dir_a).await.unwrap();
	assert!(
		client
			.dir_exists(&test_dir.into(), "a")
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn dir_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = client.create_dir(&test_dir.into(), "a").await.unwrap();
	let dir_b = client.create_dir(&test_dir.into(), "b").await.unwrap();

	assert!(
		client
			.list_dir(&(&dir_b).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap()
			.0
			.is_empty()
	);

	client.move_dir(&mut dir_a, &(&dir_b).into()).await.unwrap();
	assert!(
		client
			.list_dir(&(&dir_b).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap()
			.0
			.contains(&dir_a)
	);
}

#[shared_test_runtime]
async fn size() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let size = Normal::dir_size(&**client, &test_dir.into(), ())
		.await
		.unwrap();
	assert_eq!(size.size, 0);
	assert_eq!(size.files, 0);
	assert_eq!(size.dirs, 0);

	client.create_dir(&test_dir.into(), "a").await.unwrap();
	time::sleep(time::Duration::from_secs(2400)).await; // ddos protection
	let size = Normal::dir_size(&**client, &test_dir.into(), ())
		.await
		.unwrap();
	assert_eq!(size.size, 0);
	assert_eq!(size.files, 0);
	assert_eq!(size.dirs, 1);

	client.create_dir(&test_dir.into(), "b").await.unwrap();
	time::sleep(time::Duration::from_secs(2400)).await; // ddos protection
	let size = Normal::dir_size(&**client, &test_dir.into(), ())
		.await
		.unwrap();
	assert_eq!(size.size, 0);
	assert_eq!(size.files, 0);
	assert_eq!(size.dirs, 2);
}

#[shared_test_runtime]
async fn dir_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let second_dir = client
		.create_dir(&test_dir.into(), "second_dir")
		.await
		.unwrap();

	let rng = &mut rand::rng();
	let dir_random_part_long = generate_random_base64_values(8, rng);
	let dir_random_part_short = generate_random_base64_values(2, rng);

	let dir_name = format!("{dir_random_part_long}{dir_random_part_short}");

	let dir = client
		.create_dir(&(&second_dir).into(), &dir_name)
		.await
		.unwrap();

	let found_items = client
		.find_item_matches_for_name(&dir_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootItemType::Dir(Cow::Owned(dir.clone())),
			format!(
				"/{}/{}",
				test_dir.name().unwrap(),
				second_dir.name().unwrap()
			)
		)]
	);

	let found_items = client
		.find_item_matches_for_name(&dir_random_part_short)
		.await
		.unwrap();

	assert!(found_items.iter().any(|(item, _)| {
		if let NonRootItemType::Dir(found_dir) = item {
			*found_dir.clone() == dir
		} else {
			false
		}
	}));
}

#[shared_test_runtime]
async fn dir_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_name = "dir";
	let mut dir = client.create_dir(&test_dir.into(), dir_name).await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), dir_name))
			.await
			.unwrap(),
		Some(NonRootFileType::Dir(Cow::Borrowed(&dir)))
	);

	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default().name("new_name").unwrap(),
		)
		.await
		.unwrap();

	assert_eq!(dir.name().unwrap(), "new_name");
	assert_eq!(
		client
			.find_item_at_path(&format!(
				"{}/{}",
				test_dir.name().unwrap(),
				dir.name().unwrap()
			))
			.await
			.unwrap(),
		Some(NonRootFileType::Dir(Cow::Borrowed(&dir)))
	);

	let created = Utc::now() - chrono::Duration::days(1);
	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default().created(Some(created)),
		)
		.await
		.unwrap();
	assert_eq!(dir.created(), Some(created.round_subsecs(3)));

	let found_dir = client.get_dir(*dir.uuid()).await.unwrap();
	assert_eq!(found_dir.created(), Some(created.round_subsecs(3)));
	assert_eq!(found_dir, dir);
}

#[shared_test_runtime]
async fn dir_favorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "test_dir")
		.await
		.unwrap();

	assert!(!dir.favorited());

	client.set_dir_favorite(&mut dir, true).await.unwrap();
	assert!(dir.favorited());

	client.set_dir_favorite(&mut dir, false).await.unwrap();
	assert!(!dir.favorited());
}

#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn dir_malformed_meta() {
	use filen_sdk_rs::fs::dir::{meta::DirectoryMeta, traits::HasDirMeta};
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let uuid = client
		.create_malformed_dir(&test_dir.into(), "malformed", "malformed meta")
		.await
		.unwrap();

	let dirs = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
		.0;
	assert!(dirs.iter().any(|d| *d.uuid() == uuid));
	assert!(matches!(dirs[0].get_meta(), DirectoryMeta::Encrypted(_)));

	let dir = client.get_dir(uuid).await.unwrap();
	assert!(matches!(dir.get_meta(), DirectoryMeta::Encrypted(_)));
}

#[shared_test_runtime]
async fn download_to_zip() {
	use std::io::{BufReader, Read, Seek};

	use filen_sdk_rs::fs::file::traits::HasFileInfo;
	use tokio_util::compat::TokioAsyncWriteCompatExt;
	use zip::{ZipArchive, extra_fields::ExtraField, read::ZipFile};

	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(&test_dir.into(), "a").await.unwrap();
	let dir_b = client.create_dir(&(&dir_a).into(), "b").await.unwrap();
	let _dir_c = client.create_dir(&test_dir.into(), "c").await.unwrap();

	let file = client
		.make_file_builder("file.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client
		.upload_file(file.into(), b"root file content")
		.await
		.unwrap();

	let file_1 = client
		.make_file_builder("file1.txt", *dir_a.uuid())
		.unwrap()
		.build();
	let file_1 = client
		.upload_file(file_1.into(), b"file 1 content")
		.await
		.unwrap();
	let file_2 = client
		.make_file_builder("file2.txt", *dir_b.uuid())
		.unwrap()
		.build();
	let file_2 = client
		.upload_file(file_2.into(), b"file 2 content")
		.await
		.unwrap();
	let file_3 = client
		.make_file_builder("file3.txt", *dir_b.uuid())
		.unwrap()
		.build();
	let file_3 = client
		.upload_file(file_3.into(), b"file 3 content")
		.await
		.unwrap();

	let tmp = std::env::temp_dir();
	let mut options = tokio::fs::OpenOptions::new();
	options.create(true).write(true).read(true).truncate(true);
	let zip_file = options
		.open(tmp.join("test.zip"))
		.await
		.unwrap()
		.compat_write();
	let zip_file = client
		.download_items_to_zip::<Normal, _>(
			&[
				NonRootFileType::File(Cow::Borrowed(&file)),
				NonRootFileType::Dir(Cow::Borrowed(&dir_a)),
			],
			zip_file,
			None::<&fn(u64, u64, u64, u64)>,
			(),
		)
		.await
		.unwrap();
	let mut zip_file = zip_file.into_inner().into_std().await;
	zip_file.seek(std::io::SeekFrom::Start(0)).unwrap();
	let mut archive = ZipArchive::new(BufReader::new(zip_file)).unwrap();
	let names = archive.file_names().collect::<Vec<_>>();
	assert!(names.contains(&"a/"));
	assert!(names.contains(&"a/b/"));
	assert!(names.contains(&"file.txt"));
	assert!(names.contains(&"a/file1.txt"));
	assert!(names.contains(&"a/b/file2.txt"));
	assert!(names.contains(&"a/b/file3.txt"));

	assert!(archive.by_name("a/").unwrap().is_dir());
	assert!(archive.by_name("a/b/").unwrap().is_dir());

	let assert_file_eq =
		|file: &mut ZipFile<BufReader<File>>, expected: &[u8], expected_file: &RemoteFile| {
			assert!(file.is_file());
			let mut buf = Vec::new();
			file.read_to_end(&mut buf).unwrap();
			assert_eq!(buf, expected);
			let extra_fields = file.extra_data_fields().collect::<Vec<_>>();
			assert!(extra_fields.len() >= 2);
			for field in &extra_fields {
				match field {
					ExtraField::ExtendedTimestamp(data) => {
						if let Some(modified) = expected_file.last_modified() {
							assert_eq!(data.mod_time(), Some(modified.timestamp() as u32));
						}
						if let Some(created) = expected_file.created() {
							assert_eq!(data.cr_time(), Some(created.timestamp() as u32));
						}
					}
					ExtraField::Ntfs(data) => {
						if let Some(modified) = expected_file.last_modified() {
							assert_eq!(
								data.mtime(),
								filen_sdk_rs::io::unix_time_to_nt_time(modified)
							);
						}
						if let Some(created) = expected_file.created() {
							assert_eq!(
								data.ctime(),
								filen_sdk_rs::io::unix_time_to_nt_time(created)
							);
						}
					}
					#[allow(unreachable_patterns)]
					_ => {}
				}
			}
		};

	assert_file_eq(
		&mut archive.by_name("file.txt").unwrap(),
		b"root file content",
		&file,
	);
	assert_file_eq(
		&mut archive.by_name("a/file1.txt").unwrap(),
		b"file 1 content",
		&file_1,
	);
	assert_file_eq(
		&mut archive.by_name("a/b/file2.txt").unwrap(),
		b"file 2 content",
		&file_2,
	);
	assert_file_eq(
		&mut archive.by_name("a/b/file3.txt").unwrap(),
		b"file 3 content",
		&file_3,
	);
}

#[shared_test_runtime]
async fn download_linked_dir_to_zip() {
	use std::io::{BufReader, Read, Seek};

	use filen_sdk_rs::{
		auth::unauth::UnauthClient,
		fs::categories::{Linked, NonRootFileType},
	};
	use tokio_util::compat::TokioAsyncWriteCompatExt;
	use zip::ZipArchive;

	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// Create a dir structure to link
	let dir_a = client
		.create_dir(&test_dir.into(), "linked_a")
		.await
		.unwrap();
	let dir_b = client
		.create_dir(&(&dir_a).into(), "linked_b")
		.await
		.unwrap();

	let file_1 = client
		.make_file_builder("link_file1.txt", *dir_a.uuid())
		.unwrap()
		.build();
	let _file_1 = client
		.upload_file(file_1.into(), b"linked file 1")
		.await
		.unwrap();
	let file_2 = client
		.make_file_builder("link_file2.txt", *dir_b.uuid())
		.unwrap()
		.build();
	let _file_2 = client
		.upload_file(file_2.into(), b"linked file 2")
		.await
		.unwrap();

	// Create a public link for the directory
	let link = client.public_link_dir(&dir_a, &|_, _| {}).await.unwrap();

	// Use a fresh UnauthClient (simulating a non-authenticated user)
	let unauth_client =
		UnauthClient::from_config(filen_sdk_rs::auth::http::ClientConfig::default()).unwrap();

	let link_uuid = link.uuid();
	let link_key = link.key_string().unwrap();
	let info = unauth_client
		.get_dir_public_link_info(link_uuid, &link_key)
		.await
		.unwrap();

	let expected_link: DirPublicLink = link.try_into().unwrap();
	assert_eq!(info.link, expected_link);

	// Download to zip using the UnauthClient
	let tmp = std::env::temp_dir();
	let mut options = tokio::fs::OpenOptions::new();
	options.create(true).write(true).read(true).truncate(true);
	let zip_file = options
		.open(tmp.join("test_linked.zip"))
		.await
		.unwrap()
		.compat_write();
	let zip_file = unauth_client
		.download_items_to_zip::<Linked, _>(
			&[NonRootFileType::Root(Cow::Borrowed(&info.root))],
			zip_file,
			None::<&fn(u64, u64, u64, u64)>,
			Cow::Borrowed(&info.link),
		)
		.await
		.unwrap();
	let mut zip_file = zip_file.into_inner().into_std().await;
	zip_file.seek(std::io::SeekFrom::Start(0)).unwrap();
	let mut archive = ZipArchive::new(BufReader::new(zip_file)).unwrap();
	let names = archive.file_names().collect::<Vec<_>>();
	assert!(
		names.contains(&"link_file1.txt"),
		"expected link_file1.txt in zip, got: {:?}",
		names
	);
	assert!(
		names.contains(&"linked_b/"),
		"expected linked_b/ in zip, got: {:?}",
		names
	);
	assert!(
		names.contains(&"linked_b/link_file2.txt"),
		"expected linked_b/link_file2.txt in zip, got: {:?}",
		names
	);

	// Verify file contents
	let mut buf = Vec::new();
	archive
		.by_name("link_file1.txt")
		.unwrap()
		.read_to_end(&mut buf)
		.unwrap();
	assert_eq!(buf, b"linked file 1");

	buf.clear();
	archive
		.by_name("linked_b/link_file2.txt")
		.unwrap()
		.read_to_end(&mut buf)
		.unwrap();
	assert_eq!(buf, b"linked file 2");
}

#[shared_test_runtime]
async fn not_found_error() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut new_dir = client
		.create_dir(&test_dir.into(), "to_be_deleted")
		.await
		.unwrap();

	client.trash_dir(&mut new_dir).await.unwrap();
	let err = client
		.list_dir(&(&new_dir).into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::FolderNotFound)
}

// ── Name validation: DirectoryMetaChanges ───────────────────────────

#[test]
fn dir_meta_changes_rejects_invalid_names() {
	assert_eq!(
		DirectoryMetaChanges::default().name("").unwrap_err(),
		EntryNameError::Empty
	);
	assert_eq!(
		DirectoryMetaChanges::default().name(".").unwrap_err(),
		EntryNameError::DotEntry
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("..").unwrap_err(),
		EntryNameError::DotEntry
	);
	assert_eq!(
		DirectoryMetaChanges::default()
			.name(" leading")
			.unwrap_err(),
		EntryNameError::LeadingSpace
	);
	assert_eq!(
		DirectoryMetaChanges::default()
			.name("trailing.")
			.unwrap_err(),
		EntryNameError::TrailingDotOrSpace
	);
	assert_eq!(
		DirectoryMetaChanges::default()
			.name("trailing ")
			.unwrap_err(),
		EntryNameError::TrailingDotOrSpace
	);
	assert!(matches!(
		DirectoryMetaChanges::default().name("a/b"),
		Err(EntryNameError::ForbiddenChar { ch: '/', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a\\b"),
		Err(EntryNameError::ForbiddenChar { ch: '\\', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a:b"),
		Err(EntryNameError::ForbiddenChar { ch: ':', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a*b"),
		Err(EntryNameError::ForbiddenChar { ch: '*', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a?b"),
		Err(EntryNameError::ForbiddenChar { ch: '?', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a\"b"),
		Err(EntryNameError::ForbiddenChar { ch: '"', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a<b"),
		Err(EntryNameError::ForbiddenChar { ch: '<', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a>b"),
		Err(EntryNameError::ForbiddenChar { ch: '>', .. })
	));
	assert!(matches!(
		DirectoryMetaChanges::default().name("a|b"),
		Err(EntryNameError::ForbiddenChar { ch: '|', .. })
	));
	assert_eq!(
		DirectoryMetaChanges::default().name("CON").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("con").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("PRN").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("AUX").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("NUL").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("COM1").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		DirectoryMetaChanges::default().name("LPT9").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert!(matches!(
		DirectoryMetaChanges::default().name(&"x".repeat(256)),
		Err(EntryNameError::TooLong { .. })
	));
}

#[test]
fn dir_meta_changes_accepts_valid_names() {
	for name in [
		"hello",
		"file.txt",
		".hidden",
		"CON.txt",
		"NUL.log",
		"COM1.dat",
		"CONSOLE",
		"NULL",
		"日本語",
		"café",
	] {
		assert!(
			DirectoryMetaChanges::default().name(name).is_ok(),
			"expected {name:?} to be accepted"
		);
	}
}

// ── Name validation: create_dir ─────────────────────────────────────

#[shared_test_runtime]
async fn create_dir_rejects_invalid_names() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let err = client.create_dir(&test_dir.into(), "").await.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::InvalidName);

	let err = client
		.create_dir(&test_dir.into(), "CON")
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::InvalidName);

	let err = client
		.create_dir(&test_dir.into(), "foo/bar")
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::InvalidName);

	let err = client
		.create_dir(&test_dir.into(), "trailing.")
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::InvalidName);
}

#[shared_test_runtime]
async fn create_dir_normalizes_nfc() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// NFD: e + combining acute accent
	let nfd_name = "caf\u{0065}\u{0301}";
	let nfc_name = "caf\u{00E9}";

	let dir = client.create_dir(&test_dir.into(), nfd_name).await.unwrap();
	assert_eq!(dir.name().unwrap(), nfc_name);
}

// ── Name validation: update_dir_metadata ────────────────────────────

#[shared_test_runtime]
async fn update_dir_meta_rejects_invalid_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "valid_name")
		.await
		.unwrap();

	assert!(DirectoryMetaChanges::default().name("").is_err());
	assert!(DirectoryMetaChanges::default().name("CON").is_err());
	assert!(DirectoryMetaChanges::default().name("a*b").is_err());

	// Valid rename should work
	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default().name("renamed").unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(dir.name().unwrap(), "renamed");
}

#[shared_test_runtime]
async fn update_dir_meta_normalizes_nfc() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "nfc_test")
		.await
		.unwrap();

	let nfd_name = "u\u{0308}ber"; // ü as u + combining diaeresis
	let nfc_name = "\u{00FC}ber"; // ü as single codepoint

	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default().name(nfd_name).unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(dir.name().unwrap(), nfc_name);
}
