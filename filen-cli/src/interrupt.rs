//! Ctrl-C, through one handler installed when the CLI starts.
//!
//! While no command runs, Ctrl-C quits the CLI, as it would without a handler. While one runs,
//! Ctrl-C cancels it ([`catch`]), unless the command handles Ctrl-C itself ([`own`]), as a
//! transfer, `mount`, `serve` and `sync` do to stop cleanly. A further Ctrl-C while a cancelled
//! command has not ended yet quits the CLI.

use std::{
	process,
	sync::{LazyLock, Mutex, PoisonError},
};

use tokio::sync::broadcast::{self, error::RecvError};

use crate::error::CliError;

/// The exit code of the CLI when Ctrl-C quit it or cancelled its command, the one a shell
/// reports for a process that SIGINT ended.
pub(crate) const EXIT_INTERRUPTED: i32 = 130;

/// Sends each Ctrl-C to the commands waiting for one. Ctrl-C is pressed far less often than the
/// receivers read, so a few slots are plenty.
static PRESSES: LazyLock<broadcast::Sender<Press>> = LazyLock::new(|| broadcast::channel(16).0);

static STATE: Mutex<State> = Mutex::new(State {
	catching: 0,
	owning: 0,
	cancelling: false,
});

/// Installs the Ctrl-C handler. Called once, when the CLI starts.
pub(crate) fn install() -> Result<(), ctrlc::Error> {
	ctrlc::set_handler(on_ctrl_c)
}

/// What the handler does on Ctrl-C.
fn on_ctrl_c() {
	match lock_state().press() {
		Action::Quit => quit(),
		Action::Send(press) => {
			// No receiver is not an error: the command that owns Ctrl-C may be ending.
			let _ = PRESSES.send(press);
		}
	}
}

fn lock_state() -> std::sync::MutexGuard<'static, State> {
	STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Quits the CLI after Ctrl-C.
fn quit() -> ! {
	// A progress bar hides the cursor, which would stay hidden in the shell.
	let term = console::Term::stderr();
	if term.is_term() {
		let _ = term.show_cursor();
	}
	log::info!("Quit by Ctrl-C");
	process::exit(EXIT_INTERRUPTED)
}

/// What the CLI is doing about Ctrl-C.
#[derive(Debug, PartialEq)]
struct State {
	/// How many commands are running that Ctrl-C cancels. While none is, Ctrl-C quits the CLI.
	catching: usize,
	/// How many commands are running that handle Ctrl-C themselves.
	owning: usize,
	/// Whether a Ctrl-C has cancelled a command that has not ended yet.
	cancelling: bool,
}

/// What a Ctrl-C does.
#[derive(Debug, PartialEq)]
enum Action {
	Quit,
	Send(Press),
}

/// A Ctrl-C, as sent to the commands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Press {
	/// Whether a command handles it itself, so the one running it does not cancel it.
	owned: bool,
}

impl State {
	fn press(&mut self) -> Action {
		if self.catching == 0 || self.cancelling {
			return Action::Quit;
		}
		let owned = self.owning > 0;
		if !owned {
			self.cancelling = true;
		}
		Action::Send(Press { owned })
	}

	fn end_catching(&mut self) {
		self.catching = self.catching.saturating_sub(1);
		if self.catching == 0 {
			self.cancelling = false;
		}
	}
}

/// Held while a command runs that Ctrl-C cancels: see [`Catching::cancelled`].
pub(crate) struct Catching {
	presses: broadcast::Receiver<Press>,
}

/// Lets Ctrl-C cancel the command about to run, rather than quit the CLI, until the returned
/// [`Catching`] is dropped.
pub(crate) fn catch() -> Catching {
	let mut state = lock_state();
	// Under the lock the handler sends under, so no Ctrl-C falls between the two.
	let presses = PRESSES.subscribe();
	state.catching += 1;
	Catching { presses }
}

impl Catching {
	/// Waits for a Ctrl-C that cancels the command: one that no command handles itself.
	pub(crate) async fn cancelled(&mut self) {
		loop {
			match self.presses.recv().await {
				Ok(Press { owned: true }) => {}
				// A Ctrl-C that cannot be told apart cancels the command, as most do.
				Ok(Press { owned: false }) | Err(RecvError::Lagged(_)) => return,
				// `PRESSES` is static, so its sender is never dropped.
				Err(RecvError::Closed) => std::future::pending().await,
			}
		}
	}
}

impl Drop for Catching {
	fn drop(&mut self) {
		lock_state().end_catching();
	}
}

/// Runs `command` until it ends or Ctrl-C cancels it, unless it handles Ctrl-C itself ([`own`]).
/// Cancelling drops `command` and fails with [`CliError::Interrupted`].
pub(crate) async fn cancellable<T>(
	command: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
	let mut catching = catch();
	tokio::select! {
		result = command => result,
		() = catching.cancelled() => Err(CliError::Interrupted.into()),
	}
}

/// Held while a command handles Ctrl-C itself: see [`Owning::pressed`].
pub(crate) struct Owning {
	presses: broadcast::Receiver<Press>,
}

/// Has the running command handle Ctrl-C itself, until the returned [`Owning`] is dropped: the
/// command is not cancelled, and waits for Ctrl-C with [`Owning::pressed`]. A command takes it
/// before it starts what it has to stop cleanly, such as a watch or a daemon: a Ctrl-C while that
/// starts waits in the returned [`Owning`] until the command asks for it.
pub(crate) fn own() -> Owning {
	let mut state = lock_state();
	let presses = PRESSES.subscribe();
	state.owning += 1;
	Owning { presses }
}

impl Owning {
	/// Waits for Ctrl-C.
	pub(crate) async fn pressed(&mut self) {
		match self.presses.recv().await {
			Ok(_) | Err(RecvError::Lagged(_)) => {}
			// `PRESSES` is static, so its sender is never dropped.
			Err(RecvError::Closed) => std::future::pending().await,
		}
	}
}

impl Drop for Owning {
	fn drop(&mut self) {
		let mut state = lock_state();
		state.owning = state.owning.saturating_sub(1);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Held by each test that touches the handler's state or its presses, which are the process's,
	/// so that no two of them run at once.
	static HANDLER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

	fn state(catching: usize, owning: usize) -> State {
		State {
			catching,
			owning,
			cancelling: false,
		}
	}

	#[test]
	fn ctrl_c_with_no_command_running_quits() {
		assert_eq!(state(0, 0).press(), Action::Quit);
	}

	#[test]
	fn ctrl_c_cancels_a_running_command() {
		let mut state = state(1, 0);
		assert_eq!(state.press(), Action::Send(Press { owned: false }));
		assert!(state.cancelling);
	}

	#[test]
	fn a_second_ctrl_c_before_the_cancelled_command_ended_quits() {
		let mut state = state(1, 0);
		state.press();
		assert_eq!(state.press(), Action::Quit);
	}

	#[test]
	fn ctrl_c_after_a_cancelled_command_ended_cancels_the_next_one() {
		let mut state = state(1, 0);
		state.press();
		state.end_catching();
		state.catching += 1;
		assert_eq!(state.press(), Action::Send(Press { owned: false }));
	}

	#[test]
	fn ctrl_c_goes_to_a_command_handling_it_every_time() {
		let mut state = state(1, 1);
		assert_eq!(state.press(), Action::Send(Press { owned: true }));
		assert_eq!(state.press(), Action::Send(Press { owned: true }));
		assert!(!state.cancelling);
	}

	#[tokio::test]
	async fn a_running_command_is_told_of_ctrl_c_that_no_command_handles_only() {
		let _handler = HANDLER.lock().await;
		let mut catching = catch();
		PRESSES.send(Press { owned: true }).unwrap();
		PRESSES.send(Press { owned: false }).unwrap();
		catching.cancelled().await;
		assert!(catching.presses.is_empty());
	}

	#[tokio::test]
	async fn ctrl_c_cancels_a_command_after_one_that_handled_it_has_ended() {
		let _handler = HANDLER.lock().await;
		// a transfer, mount or serve that handled Ctrl-C itself ran before
		drop(own());
		let command = cancellable(std::future::pending::<anyhow::Result<()>>());
		tokio::pin!(command);
		// the first poll starts the command, which then catches Ctrl-C
		assert!(futures::poll!(&mut command).is_pending());

		on_ctrl_c();

		let err = command.await.unwrap_err();
		assert_eq!(crate::error::exit_code(&err), EXIT_INTERRUPTED);
		assert_eq!(*lock_state(), state(0, 0));
	}

	#[tokio::test]
	async fn ctrl_c_while_a_command_starts_what_it_stops_waits_for_it() {
		let _handler = HANDLER.lock().await;
		let catching = catch();
		let mut ctrl_c = own();

		on_ctrl_c();

		// returns at once: the Ctrl-C pressed before it waited
		ctrl_c.pressed().await;
		drop(ctrl_c);
		drop(catching);
		assert_eq!(*lock_state(), state(0, 0));
	}
}
