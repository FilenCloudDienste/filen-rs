use std::{ffi::OsString, str::FromStr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory as _, builder::OsStr};
use clap_complete::{ArgValueCompleter, CompletionCandidate, engine::ValueCompleter};
use filen_sdk_rs::{
	auth::Client,
	fs::categories::{DirType, NonRootFileType, Normal},
};

use crate::{CliArgs, docs::get_help_topics, util::RemotePath};

#[derive(PartialEq, strum::EnumString, strum::Display)]
enum FilenArgType {
	#[strum(serialize = "file")]
	File,
	#[strum(serialize = "directory")]
	Directory,
	#[strum(serialize = "file_or_directory")]
	FileOrDirectory,
	#[strum(serialize = "help_topic")]
	HelpTopic,
}

impl FilenArgType {
	const UNINITIALIZED_COMPLETER_OUTPUT: &str = "UNINITIALIZED_COMPLETER_OUTPUT_type=";

	fn get_uninitialized_completion_output(&self) -> String {
		format!("{}{}", Self::UNINITIALIZED_COMPLETER_OUTPUT, self)
	}

	fn try_parse_completion_output(output: &str) -> Option<FilenArgType> {
		output
			.strip_prefix(Self::UNINITIALIZED_COMPLETER_OUTPUT)
			.map(|arg_type| FilenArgType::from_str(arg_type).expect("must be valid"))
	}
}

/// Custom argument value completers for clap arguments that are remote file or directory paths in the Filen drive.
/// Since the completer needs access to a Client and the current working directory, which are not available at the time of argument definition,
/// uninitialized completers are created first, and then later replaced by calling `initialize_completers_in_command`
/// when the readline is initialized and everything is available. (This seems to be the best way to do this with clap's API).
/// Also handles completing help topics.
pub(crate) struct FilenCompleter(FilenArgType, Option<CompleterContext>);

#[derive(Clone)]
struct CompleterContext {
	client: Arc<Client>,
	working_path: RemotePath,
}

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

	pub(crate) fn help_topic() -> ArgValueCompleter {
		ArgValueCompleter::new(Self(FilenArgType::HelpTopic, None))
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
					&& let Some(arg_type) = FilenArgType::try_parse_completion_output(completion)
				{
					arg.add(ArgValueCompleter::new(Self(
						arg_type,
						Some(context.clone()),
					))) // todo: is it right to just add it in addition to the placeholder completer?
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
			None => vec![self.0.get_uninitialized_completion_output().into()],
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
				let parent: DirType<'_, Normal> = match client // todo: can we infer Normal?
					.find_item_at_path(&path.parent().0)
					.await
					.context("Failed to find parent dir")?
				{
					Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
					Some(NonRootFileType::Root(root)) => DirType::Root(root),
					Some(_) => return Err(anyhow!("Parent is not a directory")),
					None => return Err(anyhow!("Parent directory not found")),
				};
				let (dirs, files) = client
					.list_dir(&parent, None::<&fn(u64, Option<u64>)>) // todo: ?
					.await
					.context("Failed to list parent directory")?;
				let mut candidates = Vec::new();
				let basename_input = path.basename().unwrap_or("");
				for dir in dirs {
					let name = dir.meta.name().unwrap_or("");
					if name.starts_with(basename_input) {
						candidates.push(format!("{}/", name));
					}
				}
				if arg_type == &FilenArgType::File || arg_type == &FilenArgType::FileOrDirectory {
					for file in files {
						let name = file.meta.name().unwrap_or("");
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
			FilenArgType::HelpTopic => Ok(get_help_topics()
				.context("Failed to get help topics")?
				.into_iter()
				.filter(|topic| topic.starts_with(input))
				.collect()),
		}
	}
}

// for InquireCompleter
pub(crate) fn completer(
	input: &str,
	client: Arc<Client>,
	working_path: &RemotePath,
) -> Vec<String> {
	let Some(args) = shlex::split(input) else {
		return Vec::new();
	};
	let args = args.iter().map(|str| str.into()).collect::<Vec<OsString>>();
	if args.is_empty() {
		return Vec::new();
	}
	let args_index = args.len();
	let mut cli = CliArgs::command();
	cli = FilenCompleter::initialize_completers_in_command(cli, client, working_path);
	match clap_complete::engine::complete(
		&mut cli,
		vec!["filen"]
			.into_iter()
			.map(OsString::from)
			.chain(args.clone())
			.collect(),
		args_index,
		std::env::current_dir().ok().as_deref(),
	) {
		Ok(candidates) => candidates
			.into_iter()
			.filter_map(|candidate| {
				let completion = candidate.get_value().to_string_lossy().to_string();
				let completion = if completion.contains(" ") {
					format!("\"{}\"", completion)
				} else {
					completion
				};
				let replace_word = args.last().unwrap();
				input
					.strip_suffix(replace_word.to_str().unwrap())
					.map(|prefix| format!("{}{}", prefix, completion))
			})
			.collect::<Vec<_>>(),
		Err(_) => Vec::new(),
	}
}

// todo: see if we can use this same autocompletion logic for shell completion as well?

#[cfg(test)]
mod tests {
	use crate::util::RemotePath;

	#[filen_macros::shared_test_runtime]
	async fn test_completer() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let root = resources.dir.clone();
		let root_path = RemotePath::new(root.meta.name().unwrap());

		let test_completer = |input: &str, expected: &[&str]| {
			let mut expected = Vec::from(expected);
			expected.sort();
			let mut completions = super::completer(input, client.clone(), &root_path);
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
		test_completer("ls d", &["ls dir1/", "ls dir2/"]);
		test_completer("ls dir1/", &["ls dir1/"]);

		// complete second arguments
		test_completer("cp some_file.txt some_d", &["cp some_file.txt some_dir/"]);

		// differentiate between files and directories
		test_completer("cat some", &["cat some_file.txt", "cat some_dir/"]); // directories should also be suggested for cat, because they might contain files that the user wants to cat
		test_completer("ls some", &["ls some_dir/"]); // ls only accepts directories

		// completion in subdirectories
		test_completer("cat dir1/f", &["cat dir1/file_in_dir1.txt"]);
		test_completer("cat dir1/s", &["cat dir1/subdir1/"]);
		test_completer(
			"cat dir1/subdir1/f",
			&["cat dir1/subdir1/file_in_subdir1.txt"],
		);

		// completion edge cases
		test_completer("ls dir1", &["ls dir1/"]);
		// non-existing path
		test_completer("ls non_existing", &[]);
		// empty input
		test_completer("ls ", &[]);

		// completions that have spaces in them
		test_completer("ls a", &["ls \"a dir with spaces/\""]);
		// todo should also be able to do this:
		//test_completer("cat \"a dir with spaces/file\"", &["cat \"a dir with spaces/file_in_dir_with_spaces.txt\""]);
	}
}
