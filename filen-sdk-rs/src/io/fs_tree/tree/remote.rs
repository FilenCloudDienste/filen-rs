use std::{borrow::Cow, collections::HashMap};

use crate::{
	Error,
	fs::{
		HasParent, HasUUID, NonRootFSObject,
		dir::{HasUUIDContents, RemoteDirectory},
		file::RemoteFile,
	},
	io::{WalkError, fs_tree::entry::remote::RemoteFSObjectEntry},
};

use filen_types::fs::ParentUuid;
use uuid::Uuid;

pub(crate) struct WalkDirFromHashMap {
	map: HashMap<Uuid, Vec<NonRootFSObject<'static>>>,
	stack: Vec<Uuid>,
}

impl WalkDirFromHashMap {
	pub fn new(
		root: &impl HasUUIDContents,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
	) -> Result<Self, Error> {
		let mut map: HashMap<Uuid, Vec<NonRootFSObject<'static>>> = HashMap::new();
		for dir in dirs {
			let ParentUuid::Uuid(parent_uuid) = dir.parent() else {
				return Err(Error::custom(
					crate::ErrorKind::Internal,
					format!(
						"WalkDirFromHashMap::new encountered directory with non-UUID parent {:?} should be impossible",
						dir.parent()
					),
				));
			};
			map.entry(Uuid::from(parent_uuid))
				.or_default()
				.push(NonRootFSObject::Dir(Cow::Owned(dir)));
		}
		for file in files {
			let ParentUuid::Uuid(parent_uuid) = file.parent() else {
				return Err(Error::custom(
					crate::ErrorKind::Internal,
					format!(
						"WalkDirFromHashMap::new encountered directory with non-UUID parent {:?} should be impossible",
						file.parent()
					),
				));
			};
			map.entry(Uuid::from(parent_uuid))
				.or_default()
				.push(NonRootFSObject::File(Cow::Owned(file)));
		}
		let stack = vec![root.uuid().into()];
		Ok(Self { map, stack })
	}
}

impl Iterator for WalkDirFromHashMap {
	type Item = Result<RemoteFSObjectEntry<'static>, WalkError>;

	fn next(&mut self) -> Option<Self::Item> {
		let current_parent = self.stack.last()?;
		let current_children = match self.map.get_mut(current_parent) {
			None => {
				self.stack.pop();
				return self.next();
			}
			Some(children) => children,
		};
		let obj = match current_children.pop() {
			None => {
				self.stack.pop();
				return self.next();
			}
			Some(obj) => obj,
		};

		let depth = self.stack.len();

		if let NonRootFSObject::Dir(dir) = &obj {
			self.stack.push(Uuid::from(dir.uuid()));
		}

		Some(Ok(RemoteFSObjectEntry::new(obj, depth)))
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::AtomicBool;

	use crate::{
		auth::{Client, StringifiedClient},
		current_allocation,
	};

	use super::*;

	#[tokio::test]
	async fn test_dfs_walk() {
		dotenv::dotenv().ok();
		env_logger::init();
		log::info!(
			"Current allocation: {:.2} MiB",
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		let client = Client::from_stringified(StringifiedClient {
			email: "endur1el@protonmail.com".to_string(),
			user_id: 108492,
			root_uuid: "1f929996-ceb3-42dd-882e-3988d5cfbba7".to_string(),
			auth_info: "e06656bbecec566fe1c718f848db6a8e7c2c94ba60a562379a897f0080d73c49".to_string(),
			private_key: "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".to_string(),
			api_key: "6WYWZ0ZcMNFoI0k7MKNwLVHrGMPUPpGPvelf7efAsAqgWppvJQzEoZbzoL9k3gk5".to_string(),
			auth_version: 2,
			max_parallel_requests: None,
			max_io_memory_usage: None,
		}).unwrap();

		let request_time = std::time::Instant::now();
		// let tmp_dir = client
		// 	.find_item_in_dir(client.root(), "Important")
		// 	.await
		// 	.unwrap()
		// 	.unwrap();

		// let dir = match tmp_dir {
		// 	NonRootFSObject::Dir(d) => d,
		// 	_ => panic!("Work should be a dir"),
		// };
		let dir = client.root();

		let (dirs, files) = client
			.list_dir_recursive(&*dir, &mut |current_bytes, total_bytes| {
				log::info!(
					"Listing progress: {} bytes{}",
					current_bytes,
					if let Some(total) = total_bytes {
						format!(" / {} bytes", total)
					} else {
						"".to_string()
					}
				);
			})
			.await
			.unwrap();

		println!(
			"Total dirs: {}, files: {}, took {:.2} s, current allocation: {:.2} MiB",
			dirs.len(),
			files.len(),
			request_time.elapsed().as_secs_f64(),
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		let time = std::time::Instant::now();

		let mut iter = WalkDirFromHashMap::new(dir, dirs, files).unwrap();

		// while let Some(item) = iter.next() {
		// 	match item {
		// 		Ok(entry) => {
		// 			let depth = entry.depth();
		// 			log::info!("Walked to depth {}: {:?}", depth, entry.into_obj().name());
		// 		}
		// 		Err(e) => {
		// 			log::error!("Error during walk: {}", e);
		// 		}
		// 	}
		// }

		println!(
			"iter built took {:.2} s, current allocation: {:.2} MiB",
			time.elapsed().as_secs_f64(),
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		let (tree, stats) = super::super::build_fs_tree(
			iter,
			&mut |errors| {
				log::error!("errors while building fs tree: {:?}", errors);
			},
			&mut |dirs, files, bytes| {
				log::info!(
					"Build FS tree progress - dirs: {}, files: {}, bytes: {} (allocated: {:.2} MiB)",
					dirs,
					files,
					bytes,
					current_allocation() as f64 / 1024.0 / 1024.0
				);
			},
			&AtomicBool::new(false),
		)
		.unwrap();

		let (dirs, files, bytes) = stats.snapshot();

		log::info!(
			"Finished building FS tree: dirs: {}, files: {}, bytes: {} (elapsed: {:.2} s), allocated: {:.2} MiB",
			dirs,
			files,
			bytes,
			time.elapsed().as_secs_f64(),
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		log::info!("root children: {:?}", tree.root_children());

		for child in tree.list_children(tree.root_children()) {
			let name = tree.get_name(child);
			println!(" - {:?} - name {}", child, name);
			// if let crate::io::fs_tree::Entry::Dir(dir_entry) = child {
			// 	println!("   dir children:");
			// 	// for grandchild in tree.list_children(dir_entry.children_info()) {
			// 	// 	let name = tree.get_name(grandchild);
			// 	// 	println!("     - {:?} - name {}", grandchild, name);
			// 	// }
			// }
		}

		let now = std::time::Instant::now();
		let root = std::path::PathBuf::from("/Users/end/Documents/tmp/test_mkdir");
		std::fs::create_dir_all(&root).unwrap();
		let dfs = tree.dfs_iter_with_path(&root);

		for (entry, path) in dfs {
			if let crate::io::fs_tree::Entry::Dir(entry) = entry {
				let dir = entry.extra_data();
				match std::fs::create_dir(&path) {
					Ok(_) => {
						dir.set_dir_times(&path).expect("able to set dir times");
					}
					Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
						log::info!("Dir {:?} already exists", path);
					}
					Err(e) => {
						log::error!("Failed to create dir {:?}: {}", path, e);
					}
				}
			}
		}

		println!(
			"DFS walk and mkdir took {:.2} s, current allocation: {:.2} MiB",
			now.elapsed().as_secs_f64(),
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		// std::hint::black_box(tree);

		// let client = Arc::new(client);

		// let (tree, stats) = build_fs_tree_from_remote_iterator(
		// 	Arc::clone(&client),
		// 	client.root(),
		// 	&mut |errors| {
		// 		for error in errors {
		// 			log::error!("Error during walk: {}", error);
		// 		}
		// 	},
		// 	&mut |dirs, files, bytes| {
		// 		println!(
		// 			"Walk progress - dirs: {}, files: {}, bytes: {} (elapsed: {:.2} s), allocated: {:.2} MiB",
		// 			dirs,
		// 			files,
		// 			bytes,
		// 			walk_time.elapsed().as_millis() as f64 / 1000.0,
		// 			current_allocation() as f64 / 1024.0 / 1024.0
		// 		);
		// 	},
		// )
		// .await
		// .unwrap();

		// println!(
		// 	"Finished building FS tree: dirs: {}, files: {}, bytes: {} (elapsed: {:.2} s), allocated: {:.2} MiB",
		// 	stats.dirs,
		// 	stats.files,
		// 	stats.bytes,
		// 	walk_time.elapsed().as_millis() as f64 / 1000.0,
		// 	current_allocation() as f64 / 1024.0 / 1024.0
		// );
		// std::hint::black_box(tree);
	}
}
