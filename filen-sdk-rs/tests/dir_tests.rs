use std::{
	borrow::Cow,
	fs::File,
	io::{BufReader, Read, Seek},
};

use chrono::{SubsecRound, Utc};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	crypto::shared::generate_random_base64_values,
	fs::{
		FSObject, HasName, HasRemoteInfo, HasUUID, NonRootFSObject, UnsharedFSObject,
		client_impl::ObjectOrRemainingPath,
		dir::{UnsharedDirectoryType, meta::DirectoryMetaChanges},
		file::{RemoteFile, traits::HasFileInfo},
	},
};
use tokio::time;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use zip::{ExtraField, read::ZipFile};

#[shared_test_runtime]
async fn create_list_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(test_dir, "test_dir".to_string())
		.await
		.unwrap();
	assert_eq!(dir.name().unwrap(), "test_dir");

	let (dirs, _) = client.list_dir(test_dir).await.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}

	client.trash_dir(&mut dir).await.unwrap();
}

#[shared_test_runtime]
async fn find_at_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b".to_string()).await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c".to_string()).await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/b/c", test_dir.name().unwrap()))
			.await
			.unwrap(),
		Some(FSObject::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/bc", test_dir.name().unwrap()))
			.await
			.unwrap(),
		None
	);

	let path = format!("{}/a/b/c", test_dir.name().unwrap());

	let items = client.get_items_in_path(&path).await.unwrap();

	assert_eq!(items.0.len(), 4);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
	);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
	);
	assert!(
		!items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_c)))
	);
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(UnsharedFSObject::Dir(dir)) if *dir == dir_c
	));

	let items = client
		.get_items_in_path_starting_at("b/c", UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
		.await
		.unwrap();
	assert_eq!(items.0.len(), 2);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
	);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
	);
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(UnsharedFSObject::Dir(dir)) if *dir == dir_c
	));

	let resp = client
		.get_items_in_path_starting_at("c/d/e", UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
		.await
		.unwrap();
	// Expecting None because "c/d/e" does not exist in the path
	// and the last item in dirs be the directory "c"
	match resp {
		(mut dirs, ObjectOrRemainingPath::RemainingPath(path)) => match dirs.pop() {
			Some(UnsharedDirectoryType::Dir(dir)) if *dir == dir_c => {
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
		Some(Into::<FSObject<'_>>::into(nested_dir.clone())),
		client.find_item_at_path(&path).await.unwrap()
	);

	let nested_dir = client
		.find_or_create_dir_starting_at(nested_dir, "d/e")
		.await
		.unwrap();
	assert_eq!(
		Some(Into::<FSObject<'_>>::into(nested_dir.clone())),
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

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b".to_string()).await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c".to_string()).await.unwrap();

	let (dirs, _) = client.list_dir_recursive(test_dir).await.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));
}

#[shared_test_runtime]
async fn exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());

	let mut dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		client.dir_exists(test_dir, "a").await.unwrap().as_ref()
	);

	client.trash_dir(&mut dir_a).await.unwrap();
	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());
}

#[shared_test_runtime]
async fn dir_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(test_dir, "b".to_string()).await.unwrap();

	assert!(client.list_dir(&dir_b).await.unwrap().0.is_empty());

	client.move_dir(&mut dir_a, &dir_b).await.unwrap();
	assert!(client.list_dir(&dir_b).await.unwrap().0.contains(&dir_a));
}

#[shared_test_runtime]
async fn size() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 0
		}
	);

	client.create_dir(test_dir, "a".to_string()).await.unwrap();
	time::sleep(time::Duration::from_secs(1200)).await; // ddos protection
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 1
		}
	);

	client.create_dir(test_dir, "b".to_string()).await.unwrap();
	time::sleep(time::Duration::from_secs(1200)).await; // ddos protection
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 2
		}
	);
}

#[shared_test_runtime]
async fn dir_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let second_dir = client
		.create_dir(test_dir, "second_dir".to_string())
		.await
		.unwrap();

	let dir_random_part_long = generate_random_base64_values(16);
	let dir_random_part_short = generate_random_base64_values(2);

	let dir_name = format!("{dir_random_part_long}{dir_random_part_short}");

	let dir = client.create_dir(&second_dir, dir_name).await.unwrap();

	let found_items = client
		.find_item_matches_for_name(dir_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootFSObject::Dir(Cow::Owned(dir.clone())),
			format!(
				"/{}/{}",
				test_dir.name().unwrap(),
				second_dir.name().unwrap()
			)
		)]
	);

	let found_items = client
		.find_item_matches_for_name(dir_random_part_short)
		.await
		.unwrap();

	assert!(found_items.iter().any(|(item, _)| {
		if let NonRootFSObject::Dir(found_dir) = item {
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
	let mut dir = client
		.create_dir(test_dir, dir_name.to_string())
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name().unwrap(), dir_name))
			.await
			.unwrap(),
		Some(FSObject::Dir(Cow::Borrowed(&dir)))
	);

	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default()
				.name("new_name".to_string())
				.unwrap(),
		)
		.await
		.unwrap();

	assert_eq!(dir.name().unwrap(), "new_name");
	assert_eq!(
		client
			.find_item_at_path(format!(
				"{}/{}",
				test_dir.name().unwrap(),
				dir.name().unwrap()
			))
			.await
			.unwrap(),
		Some(FSObject::Dir(Cow::Borrowed(&dir)))
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
		.create_dir(test_dir, "test_dir".to_string())
		.await
		.unwrap();

	assert!(!dir.favorited());

	client.set_favorite(&mut dir, true).await.unwrap();
	assert!(dir.favorited());

	client.set_favorite(&mut dir, false).await.unwrap();
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
		.create_malformed_dir(test_dir, "malformed", "malformed meta".to_string())
		.await
		.unwrap();

	let dirs = client.list_dir(test_dir).await.unwrap().0;
	assert!(dirs.iter().any(|d| *d.uuid() == uuid));
	assert!(matches!(dirs[0].get_meta(), DirectoryMeta::Encrypted(_)));

	let dir = client.get_dir(uuid).await.unwrap();
	assert!(matches!(dir.get_meta(), DirectoryMeta::Encrypted(_)));
}

#[shared_test_runtime]
async fn download_to_zip() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b".to_string()).await.unwrap();
	let _dir_c = client.create_dir(test_dir, "c".to_string()).await.unwrap();

	let file = client.make_file_builder("file.txt", test_dir).build();
	let file = client
		.upload_file(file.into(), b"root file content")
		.await
		.unwrap();

	let file_1 = client.make_file_builder("file1.txt", &dir_a).build();
	let file_1 = client
		.upload_file(file_1.into(), b"file 1 content")
		.await
		.unwrap();
	let file_2 = client.make_file_builder("file2.txt", &dir_b).build();
	let file_2 = client
		.upload_file(file_2.into(), b"file 2 content")
		.await
		.unwrap();
	let file_3 = client.make_file_builder("file3.txt", &dir_b).build();
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
		.download_items_to_zip(
			&[
				UnsharedFSObject::File(Cow::Borrowed(&file)),
				UnsharedFSObject::Dir(Cow::Borrowed(&dir_a)),
			],
			zip_file,
			None::<&fn(u64, u64, u64, u64)>,
		)
		.await
		.unwrap();
	let mut zip_file = zip_file.into_inner().into_std().await;
	zip_file.seek(std::io::SeekFrom::Start(0)).unwrap();
	let mut archive = zip::ZipArchive::new(BufReader::new(zip_file)).unwrap();
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
