use std::{fmt::Display, sync::Arc};

use anyhow::{Context, Result, anyhow};
use clap::builder::OsStr;
use clap_complete::{ArgValueCompleter, CompletionCandidate, engine::ValueCompleter};
use filen_sdk_rs::{
	auth::Client,
	fs::{FSObject, HasName, dir::DirectoryType},
};

use crate::util::RemotePath;

#[derive(PartialEq)]
pub(crate) enum FilenArgType {
	File,
	Directory,
	FileOrDirectory,
}

impl From<&str> for FilenArgType {
	fn from(s: &str) -> Self {
		match s {
			"file" => FilenArgType::File,
			"directory" => FilenArgType::Directory,
			"file_or_directory" => FilenArgType::FileOrDirectory,
			_ => panic!("Unknown FilenArgType: {}", s), // todo: style?
		}
	}
}

impl Display for FilenArgType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let s = match self {
			FilenArgType::File => "file",
			FilenArgType::Directory => "directory",
			FilenArgType::FileOrDirectory => "file_or_directory",
		};
		write!(f, "{}", s)
	}
}

/// Custom argument value completers for clap arguments that are remote file or directory paths in the Filen drive.
/// Since the completer needs access to a Client and the current working directory, which are not available at the time of argument definition,
/// uninitialized completers are created first, and then later replaced by calling `initialize_completers_in_command`
/// when the readline is initialized and everything is available. (This seems to be the best way to do this with clap's API).
pub(crate) struct FilenCompleter(FilenArgType, Option<CompleterContext>);

#[derive(Clone)]
struct CompleterContext {
	pub(crate) client: Arc<Client>,
	pub(crate) working_path: RemotePath,
}

const UNINITIALIZED_COMPLETER_OUTPUT: &str = "UNINITIALIZED_COMPLETER_OUTPUT_type=";

impl FilenCompleter {
	pub(crate) fn file() -> ArgValueCompleter {
		ArgValueCompleter::new(Self(FilenArgType::File, None))
	}

	pub(crate) fn directory() -> ArgValueCompleter {
		ArgValueCompleter::new(Self(FilenArgType::Directory, None))
	}

	pub(crate) fn file_or_directory() -> ArgValueCompleter {
		ArgValueCompleter::new(Self(FilenArgType::FileOrDirectory, None))
	}

	pub(crate) fn initialize_completers_in_command(
		command: clap::Command,
		client: Arc<Client>,
		working_path: &RemotePath,
	) -> clap::Command {
		let context = CompleterContext {
			client,
			working_path: working_path.clone(),
		};
		command.mut_subcommands(|subcommand| {
			subcommand.mut_args(|arg| {
				if let Some(completer) = arg.get::<ArgValueCompleter>()
					&& let Some(Some(completion)) = completer
						.complete(&OsStr::default())
						.first()
						.map(|c| c.get_value().to_str())
					&& let Some(arg_type) = completion.strip_prefix(UNINITIALIZED_COMPLETER_OUTPUT)
				{
					arg.add(ArgValueCompleter::new(Self(
						arg_type.into(),
						Some(context.clone()),
					)))
				} else {
					arg
				}
			})
		})
	}
}

impl ValueCompleter for FilenCompleter {
	fn complete(&self, _input: &std::ffi::OsStr) -> Vec<clap_complete::CompletionCandidate> {
		match self.1 {
			None => vec![CompletionCandidate::new(format!(
				"{}{}",
				UNINITIALIZED_COMPLETER_OUTPUT, self.0
			))],
			Some(ref context) => tokio::task::block_in_place(|| {
				match tokio::runtime::Handle::current().block_on(Self::complete(
					&self.0,
					&context.client,
					&context.working_path,
					_input.to_str().unwrap_or(""),
				)) {
					Ok(candidates) => candidates
						.into_iter()
						.map(CompletionCandidate::new)
						.collect(),
					Err(e) => {
						eprintln!("Error during completion: {}", e); // todo
						vec![]
					}
				}
			}),
		}
	}
}

impl FilenCompleter {
	async fn complete(
		arg_type: &FilenArgType,
		client: &Client,
		working_path: &RemotePath,
		input: &str,
	) -> Result<Vec<String>> {
		match arg_type {
			FilenArgType::File | FilenArgType::Directory | FilenArgType::FileOrDirectory => {
				let path = working_path.navigate(input);
				let parent = match client
					.find_item_at_path(&path.parent().0)
					.await
					.context("Failed to find parent dir")?
				{
					Some(FSObject::Dir(dir)) => DirectoryType::Dir(dir),
					Some(FSObject::Root(root)) => DirectoryType::Root(root),
					Some(FSObject::RootWithMeta(root)) => DirectoryType::RootWithMeta(root),
					Some(_) => return Err(anyhow!("Parent is not a directory")),
					None => return Err(anyhow!("Parent directory not found")),
				};
				let (dirs, files) = client
					.list_dir(&parent)
					.await
					.context("Failed to list parent directory")?;
				let mut candidates = Vec::new();
				let basename_input = path.basename().unwrap_or("");
				for dir in dirs {
					let name = dir.name().unwrap_or("");
					if name.starts_with(basename_input) {
						candidates.push(format!("{}/", name));
					}
				}
				if arg_type == &FilenArgType::File || arg_type == &FilenArgType::FileOrDirectory {
					for file in files {
						let name = file.name().unwrap_or("");
						if name.starts_with(basename_input) {
							candidates.push(name.to_string());
						}
					}
				}
				Ok(candidates
					.iter()
					.map(|c| {
						input
							.strip_suffix(basename_input)
							.unwrap_or_default()
							.to_string() + c
					})
					.collect())
				// todo: think really hard about all the stripping etc. happening here (does it handle ".." correctly?)
			}
		}
	}
}

// todo: see if we can use this same autocompletion logic for shell completion as well?
