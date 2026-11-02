//! [cli-doc] exit-codes
//! The Filen CLI exits with:
//! - `0` when the command succeeded,
//! - `1` when it failed, with the error printed to stderr,
//! - `2` when the command line was invalid, such as an unknown command or flags that do not go together,
//! - the exit code of Rclone when `filen rclone ...` failed in Rclone.

use std::{error::Error, fmt};

use inquire::InquireError;

/// An error the CLI reports to the user as it is, with the exit code it maps to. Any other error
/// in a command's result is unexpected: it is reported as a possible bug and exits with code 1.
///
/// It stays recognizable when wrapped with `.context(..)`: [`find_cli_error`] looks through the
/// whole chain.
#[derive(Debug)]
pub(crate) enum CliError {
	/// The command could not do what was asked; exit code 1.
	Failure(String),
	/// The command line was invalid; exit code 2.
	Usage(String),
	/// The terminal could not show a prompt or read its answer; exit code 1.
	TerminalInput(InquireError),
	/// A child process (Rclone) exited with this code and reported its own error; its code is the
	/// CLI's exit code.
	ChildExit(i32),
}

/// Exit code of a command that failed.
pub(crate) const EXIT_FAILURE: i32 = 1;
/// Exit code of an invalid command line, the one clap exits with on an invalid command line.
pub(crate) const EXIT_USAGE: i32 = 2;

impl CliError {
	pub(crate) fn exit_code(&self) -> i32 {
		match self {
			CliError::Failure(_) | CliError::TerminalInput(_) => EXIT_FAILURE,
			CliError::Usage(_) => EXIT_USAGE,
			CliError::ChildExit(code) => *code,
		}
	}
}

impl fmt::Display for CliError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			CliError::Failure(msg) | CliError::Usage(msg) => f.write_str(msg),
			CliError::TerminalInput(_) => f.write_str("Failed to read input prompt"),
			CliError::ChildExit(code) => write!(f, "Exited with code {code}"),
		}
	}
}

impl Error for CliError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			CliError::TerminalInput(e) => Some(e),
			CliError::Failure(_) | CliError::Usage(_) | CliError::ChildExit(_) => None,
		}
	}
}

/// The [`CliError`] in `err`'s chain, if any: the outermost one, should there be several.
pub(crate) fn find_cli_error(err: &anyhow::Error) -> Option<&CliError> {
	err.chain()
		.find_map(|cause| cause.downcast_ref::<CliError>())
}

/// The exit code the CLI exits with after `err`.
pub(crate) fn exit_code(err: &anyhow::Error) -> i32 {
	find_cli_error(err).map_or(EXIT_FAILURE, CliError::exit_code)
}

#[cfg(test)]
mod tests {
	use anyhow::Context as _;

	use super::*;

	fn failure() -> anyhow::Result<()> {
		Err(CliError::Failure("No such file: /a".to_owned()).into())
	}

	#[test]
	fn a_failure_wrapped_in_context_is_still_a_failure() {
		let err = failure().context("Failed to copy").unwrap_err();

		let cli_error = find_cli_error(&err).unwrap();
		assert!(matches!(cli_error, CliError::Failure(msg) if msg == "No such file: /a"));
		assert_eq!(exit_code(&err), 1);
		assert_eq!(format!("{err:#}"), "Failed to copy: No such file: /a");
	}

	#[test]
	fn an_error_without_a_cli_error_is_unexpected() {
		let err = anyhow::anyhow!("connection reset");

		assert!(find_cli_error(&err).is_none());
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn a_usage_error_wrapped_in_context_exits_with_code_2() {
		let err = anyhow::Error::from(CliError::Usage(
			"Give both --user and --password, or neither".to_owned(),
		))
		.context("Failed to serve");

		assert_eq!(exit_code(&err), 2);
	}

	#[test]
	fn a_child_exit_code_passes_through() {
		let err = anyhow::Error::from(CliError::ChildExit(7));

		assert_eq!(exit_code(&err), 7);
	}

	#[test]
	fn a_message_that_looks_like_a_failure_is_not_one() {
		let err = anyhow::anyhow!("✘ Exit with code 3");

		assert!(find_cli_error(&err).is_none());
		assert_eq!(exit_code(&err), 1);
	}
}
