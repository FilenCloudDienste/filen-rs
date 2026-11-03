use std::{borrow::Cow, ffi::OsString, iter, mem, str::FromStr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory as _, builder::OsStr};
use clap_complete::{ArgValueCompleter, CompletionCandidate, engine::ValueCompleter};
use filen_sdk_rs::{
	auth::Client,
	fs::categories::{DirType, NonRootFileType, Normal},
};

use crate::{CliArgs, docs::get_help_topics, util::RemotePath};

#[derive(PartialEq, strum::EnumString, strum::Display, Clone)]
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
	/// Whether to block and wait for the result of the async completion task.
	/// Normally, use async autocomplete.
	await_result: bool,
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
		await_result: bool,
	) -> clap::Command {
		let context = CompleterContext {
			client,
			working_path: working_path.clone(),
			await_result,
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
	/// The obscure way async data fetching is handled here is because clap's and inquire's APIs are not designed for async completions.
	/// Our fork of inquire contains a workaround that still requires handling it like this on the call site.
	/// Basically, when a completion is needed, an empty result is returned first, and a task to fetch the data is started in the background.
	/// inquire then continually queries the completer if new results are available, and when the data fetching future has completed,
	/// its results are saved in the dict and picked up by the next time the completer is called.
	/// By doing this, typing is not blocked by completions, but they are still available soon after.
	fn complete(&self, input: &std::ffi::OsStr) -> Vec<clap_complete::CompletionCandidate> {
		match self.1 {
			None => vec![self.0.get_uninitialized_completion_output().into()],
			Some(ref context) => tokio::task::block_in_place(|| {
				let runtime = tokio::runtime::Handle::current();
				runtime.block_on(async {
					let completion_key = CompletionKey {
						working_path: context.working_path.clone(),
						input: input.to_string_lossy().to_string(),
					};
					let mut async_completions = ASYNC_COMPLETIONS.lock().await;
					async_completions
						.pending_completion_tasks
						.retain(|key, _| key == &completion_key);
					if let Some(completions) =
						async_completions.fetched_completions.get(&completion_key)
					{
						completions.iter().map(CompletionCandidate::new).collect()
					} else {
						if let Some(handle) = async_completions
							.pending_completion_tasks
							.get(&completion_key)
						{
							if handle.is_finished() || context.await_result {
								let handle = async_completions
									.pending_completion_tasks
									.remove(&completion_key)
									.unwrap();
								match handle.await {
									Ok(Ok(completions)) => completions
										.into_iter()
										.map(CompletionCandidate::new)
										.collect(),
									Ok(Err(e)) => {
										log::error!("Error in completion task: {:?}", e);
										vec![]
									}
									Err(e) => {
										log::error!(
											"Error in completion task while joining: {:?}",
											e
										);
										vec![]
									}
								}
							} else {
								vec![]
							}
						} else {
							let arg_type = self.0.clone();
							let context = context.clone();
							let input = input.to_os_string();
							let handle = runtime.spawn(async move {
								Self::complete(
									arg_type,
									&context.client,
									&context.working_path,
									input.to_str().unwrap_or(""),
								)
								.await
							});
							if context.await_result {
								match handle.await {
									Ok(Ok(completions)) => completions
										.into_iter()
										.map(CompletionCandidate::new)
										.collect(),
									Ok(Err(e)) => {
										log::error!("Error in completion task: {:?}", e);
										vec![]
									}
									Err(e) => {
										log::error!(
											"Error in completion task while joining: {:?}",
											e
										);
										vec![]
									}
								}
							} else {
								async_completions
									.pending_completion_tasks
									.insert(completion_key.clone(), handle);
								vec![]
							}
						}
					}
				})
			}),
		}
	}
}

#[derive(PartialEq, Eq, Hash, Debug, Clone)]
struct CompletionKey {
	working_path: RemotePath,
	input: String,
}
struct AsyncCompletions {
	pending_completion_tasks:
		std::collections::HashMap<CompletionKey, tokio::task::JoinHandle<Result<Vec<String>>>>,
	fetched_completions: std::collections::HashMap<CompletionKey, Vec<String>>,
}
static ASYNC_COMPLETIONS: std::sync::LazyLock<tokio::sync::Mutex<AsyncCompletions>> =
	std::sync::LazyLock::new(|| {
		tokio::sync::Mutex::new(AsyncCompletions {
			pending_completion_tasks: std::collections::HashMap::new(),
			fetched_completions: std::collections::HashMap::new(),
		})
	});

pub(crate) fn async_completions_available() -> bool {
	tokio::task::block_in_place(|| {
		tokio::runtime::Handle::current().block_on(async {
			let async_completions = ASYNC_COMPLETIONS.lock().await;

			async_completions
				.pending_completion_tasks
				.values()
				.any(|handle| handle.is_finished())
		})
	})
}

impl FilenCompleter {
	async fn complete(
		arg_type: FilenArgType,
		client: &Client,
		working_path: &RemotePath,
		input: &str,
	) -> Result<Vec<String>> {
		match arg_type {
			FilenArgType::File | FilenArgType::Directory | FilenArgType::FileOrDirectory => {
				let basename_input = input
					.rsplit_once('/')
					.map(|(_, name)| name)
					.unwrap_or(input);
				let input_before_basename = input.strip_suffix(basename_input).unwrap_or("");
				if basename_input == "." || basename_input == ".." {
					return Ok(vec![]);
				}
				let path_before_basename = working_path.navigate(input_before_basename);

				let parent: DirType<'_, Normal> =
					match client // todo: can we infer Normal?
						.find_item_at_path(&path_before_basename.0)
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
				for dir in dirs {
					let name = dir.meta.name().unwrap_or("");
					if name.starts_with(basename_input) {
						candidates.push(name.to_string());
					}
				}
				if arg_type == FilenArgType::File || arg_type == FilenArgType::FileOrDirectory {
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
			}
			FilenArgType::HelpTopic => Ok(get_help_topics()
				.context("Failed to get help topics")?
				.into_iter()
				.collect::<std::collections::HashSet<_>>()
				.into_iter()
				.filter(|topic| topic.starts_with(input))
				.collect()),
		}
	}
}

/// A quote that a shell word opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
	Single,
	Double,
}

/// REPL input split into shell words as the REPL's line parser (`shlex`) splits a line, except
/// that the last word may be unfinished: its quote still open, or a backslash at its end.
#[derive(Debug, PartialEq, Eq)]
struct SplitInput<'a> {
	/// The input before the last word, as typed.
	prefix: &'a str,
	/// Every word with its quotes and escapes removed, the last one included even when empty.
	words: Vec<String>,
	/// The first quote the last word opened, open or closed.
	last_word_quote: Option<Quote>,
}

impl SplitInput<'_> {
	/// Whether nothing of the last word was typed yet, as after a space.
	fn last_word_is_untyped(&self) -> bool {
		self.last_word_quote.is_none() && self.words.last().is_none_or(String::is_empty)
	}
}

/// Splits `input` into shell words, keeping where the last one starts. A quote that is still open
/// runs to the end of the input, and a backslash at the end is dropped. A `#` that starts a word
/// comments out the line up to the next newline, so a comment the user is typing leaves the last
/// word untyped.
fn split_input(input: &str) -> SplitInput<'_> {
	let mut words = Vec::new();
	let mut word = String::new();
	// where the word being read starts, `None` between words
	let mut word_start = None;
	let mut open_quote = None;
	let mut last_word_quote = None;
	let mut chars = input.char_indices();
	while let Some((i, c)) = chars.next() {
		match open_quote {
			Some(Quote::Single) => match c {
				'\'' => open_quote = None,
				_ => word.push(c),
			},
			Some(Quote::Double) => match c {
				'"' => open_quote = None,
				'\\' => match chars.next() {
					Some((_, escaped @ ('$' | '`' | '"' | '\\'))) => word.push(escaped),
					Some((_, '\n')) | None => {}
					Some((_, other)) => {
						word.push('\\');
						word.push(other);
					}
				},
				_ => word.push(c),
			},
			None => match c {
				' ' | '\t' | '\n' => {
					if word_start.take().is_some() {
						words.push(mem::take(&mut word));
						last_word_quote = None;
					}
				}
				// a `#` that starts a word comments out the rest of the line
				'#' if word_start.is_none() => {
					for (_, c) in chars.by_ref() {
						if c == '\n' {
							break;
						}
					}
				}
				_ => {
					word_start.get_or_insert(i);
					match c {
						'\'' | '"' => {
							let quote = if c == '"' {
								Quote::Double
							} else {
								Quote::Single
							};
							open_quote = Some(quote);
							last_word_quote.get_or_insert(quote);
						}
						'\\' => match chars.next() {
							Some((_, '\n')) | None => {}
							Some((_, escaped)) => word.push(escaped),
						},
						_ => word.push(c),
					}
				}
			},
		}
	}
	words.push(word);
	let (prefix, _) = input
		.split_at_checked(word_start.unwrap_or(input.len()))
		.expect("a word starts at a char boundary (should be impossible)");
	SplitInput {
		prefix,
		words,
		last_word_quote,
	}
}

/// Quotes `word` so that the REPL's line parser reads it back as `word`: in the `quote` the user
/// opened, or in double quotes when it holds whitespace, a quote, a backslash or starts a comment.
/// A word the user opened single quotes for but that holds one is double-quoted.
fn quote_word(word: &str, quote: Option<Quote>) -> Cow<'_, str> {
	let needs_quotes = word.starts_with('#') || word.contains([' ', '\t', '\n', '"', '\'', '\\']);
	match quote {
		None if !needs_quotes => Cow::Borrowed(word),
		Some(Quote::Single) if !word.contains('\'') => Cow::Owned(format!("'{word}'")),
		_ => {
			let mut quoted = String::with_capacity(word.len() + 2);
			quoted.push('"');
			for c in word.chars() {
				if matches!(c, '"' | '\\') {
					quoted.push('\\');
				}
				quoted.push(c);
			}
			quoted.push('"');
			Cow::Owned(quoted)
		}
	}
}

// for InquireCompleter
pub(crate) fn completer(
	input: &str,
	client: Arc<Client>,
	working_path: &RemotePath,
	await_result: bool,
) -> Vec<String> {
	let split = split_input(input);
	if split.last_word_is_untyped() {
		return Vec::new();
	}
	let args_index = split.words.len();
	let mut cli = CliArgs::command();
	cli = FilenCompleter::initialize_completers_in_command(cli, client, working_path, await_result);
	match clap_complete::engine::complete(
		&mut cli,
		iter::once("filen")
			.chain(split.words.iter().map(String::as_str))
			.map(OsString::from)
			.collect(),
		args_index,
		std::env::current_dir().ok().as_deref(),
	) {
		Ok(candidates) => candidates
			.into_iter()
			.map(|candidate| {
				let completion = candidate.get_value().to_string_lossy();
				format!(
					"{}{}",
					split.prefix,
					quote_word(&completion, split.last_word_quote)
				)
			})
			.collect(),
		Err(_) => Vec::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn words(words: &[&str]) -> Vec<String> {
		words.iter().map(|word| (*word).to_owned()).collect()
	}

	#[test]
	fn a_plain_last_word_starts_after_the_last_space() {
		assert_eq!(
			split_input("cat dir1/f"),
			SplitInput {
				prefix: "cat ",
				words: words(&["cat", "dir1/f"]),
				last_word_quote: None,
			}
		);
	}

	#[test]
	fn a_closed_quote_in_the_last_word_is_part_of_it() {
		assert_eq!(
			split_input("ls \"a dir with spaces\"/f"),
			SplitInput {
				prefix: "ls ",
				words: words(&["ls", "a dir with spaces/f"]),
				last_word_quote: Some(Quote::Double),
			}
		);
	}

	#[test]
	fn an_open_quote_runs_to_the_end_of_the_input() {
		assert_eq!(
			split_input("ls \"a dir with spaces/f"),
			SplitInput {
				prefix: "ls ",
				words: words(&["ls", "a dir with spaces/f"]),
				last_word_quote: Some(Quote::Double),
			}
		);
	}

	#[test]
	fn an_open_single_quote_keeps_its_style() {
		assert_eq!(
			split_input("cp x 'a dir"),
			SplitInput {
				prefix: "cp x ",
				words: words(&["cp", "x", "a dir"]),
				last_word_quote: Some(Quote::Single),
			}
		);
	}

	#[test]
	fn an_escaped_space_does_not_end_a_word() {
		assert_eq!(
			split_input("cat a\\ dir/f"),
			SplitInput {
				prefix: "cat ",
				words: words(&["cat", "a dir/f"]),
				last_word_quote: None,
			}
		);
	}

	#[test]
	fn a_trailing_backslash_is_dropped() {
		assert_eq!(split_input("cat a\\").words, words(&["cat", "a"]));
	}

	#[test]
	fn a_double_quote_unescapes_only_what_the_line_parser_does() {
		assert_eq!(
			split_input("cat \"a\\\"b\\\\c\\d\"").words,
			words(&["cat", "a\"b\\c\\d"])
		);
	}

	#[test]
	fn a_space_at_the_end_starts_an_empty_last_word() {
		let split = split_input("ls ");
		assert_eq!(
			split,
			SplitInput {
				prefix: "ls ",
				words: words(&["ls", ""]),
				last_word_quote: None,
			}
		);
		assert!(split.last_word_is_untyped());
	}

	#[test]
	fn an_opened_empty_quote_is_typed() {
		let split = split_input("ls \"");
		assert_eq!(split.prefix, "ls ");
		assert_eq!(split.words, words(&["ls", ""]));
		assert!(!split.last_word_is_untyped());
	}

	#[test]
	fn empty_input_is_one_untyped_word() {
		let split = split_input("");
		assert_eq!(split.prefix, "");
		assert!(split.last_word_is_untyped());
	}

	#[test]
	fn split_input_agrees_with_the_line_parser_on_finished_lines() {
		for line in [
			"cp \"a b\"/c 'd e' f\\ g",
			"cat \"x\\\"y\" 'z\\w'",
			"  ls   dir1  ",
			"ls #foo",
			"cat x #c\nls a#b",
		] {
			let mut split = split_input(line);
			if split.last_word_is_untyped() {
				split.words.pop();
			}
			assert_eq!(Some(split.words), shlex::split(line), "{line}");
		}
	}

	#[test]
	fn a_comment_being_typed_leaves_nothing_to_complete() {
		let split = split_input("cat x #fo");
		assert_eq!(split.words, words(&["cat", "x", ""]));
		assert!(split.last_word_is_untyped());
	}

	#[test]
	fn a_hash_inside_a_word_is_part_of_it() {
		assert_eq!(split_input("cat a#b").words, words(&["cat", "a#b"]));
	}

	#[test]
	fn a_word_without_special_characters_is_not_quoted() {
		assert_eq!(quote_word("dir1/file.txt", None), "dir1/file.txt");
	}

	#[test]
	fn a_word_with_a_space_is_double_quoted() {
		assert_eq!(
			quote_word("a dir with spaces/f.txt", None),
			"\"a dir with spaces/f.txt\""
		);
	}

	#[test]
	fn a_word_keeps_the_quote_the_user_opened() {
		assert_eq!(quote_word("dir1", Some(Quote::Double)), "\"dir1\"");
		assert_eq!(quote_word("a dir", Some(Quote::Single)), "'a dir'");
	}

	#[test]
	fn a_single_quote_in_a_single_quoted_word_switches_to_double_quotes() {
		assert_eq!(quote_word("it's", Some(Quote::Single)), "\"it's\"");
	}

	#[test]
	fn quotes_and_backslashes_are_escaped_in_double_quotes() {
		assert_eq!(quote_word("a\"b\\c", None), "\"a\\\"b\\\\c\"");
	}

	#[test]
	fn a_word_starting_a_comment_is_quoted() {
		assert_eq!(quote_word("#notes", None), "\"#notes\"");
	}

	#[test]
	fn a_quoted_word_reads_back_as_the_word() {
		for (word, quote) in [
			("a dir/f", None),
			("a\"b\\c d", None),
			("it's here", Some(Quote::Single)),
			("x y", Some(Quote::Double)),
			("#x", None),
		] {
			let line = format!("cat {}", quote_word(word, quote));
			assert_eq!(shlex::split(&line), Some(words(&["cat", word])), "{line}");
		}
	}
}
