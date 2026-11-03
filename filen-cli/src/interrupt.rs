//! Ctrl-C, through one handler installed when the CLI starts.
//!
//! While no command runs, Ctrl-C quits the CLI, as it would without a handler. While one runs,
//! Ctrl-C cancels it ([`catch`]), unless the command handles Ctrl-C itself ([`own`]), as a
//! transfer, `mount`, `serve` and `sync` do to stop cleanly. A further Ctrl-C while a cancelled
//! command has not ended yet, or while a command stops after Ctrl-C ([`Owning::into_stopping`]),
//! quits the CLI.
//!
//! A closed stdout ([`close_stdout`]), as `head` closes it once it has read its lines, ends the
//! CLI the same way, once: the running command is cancelled, or stopped as Ctrl-C stops it, and
//! the CLI exits with 0, as the reader has what it wanted.

use std::{
	process,
	sync::{LazyLock, Mutex, PoisonError},
};

use tokio::sync::broadcast::{
	self,
	error::{RecvError, TryRecvError},
};

use crate::{error::CliError, ui::UI};

/// The exit code of the CLI when Ctrl-C quit it or cancelled its command, the one a shell
/// reports for a process that SIGINT ended.
pub(crate) const EXIT_INTERRUPTED: i32 = 130;

/// Sends each Ctrl-C to the commands waiting for one. Ctrl-C is pressed far less often than the
/// receivers read, so a few slots are plenty.
static PRESSES: LazyLock<broadcast::Sender<Press>> = LazyLock::new(|| broadcast::channel(16).0);

static STATE: Mutex<State> = Mutex::new(State {
	catching: 0,
	owning: 0,
	stopping: 0,
	cancelling: false,
	stdout_closed: false,
});

/// Installs the Ctrl-C handler. Called once, when the CLI starts.
pub(crate) fn install() -> Result<(), ctrlc::Error> {
	ctrlc::set_handler(on_ctrl_c)
}

/// What the handler does on Ctrl-C.
fn on_ctrl_c() {
	act(lock_state().press());
}

/// Ends the running command, or the CLI if none runs, as Ctrl-C does, after a write to stdout
/// found it closed. Only the first call does: the command may print more while it stops.
pub(crate) fn close_stdout() {
	act(lock_state().close_stdout());
}

/// Whether stdout was closed ([`close_stdout`]), so the CLI exits with 0 and runs no further
/// command.
pub(crate) fn stdout_closed() -> bool {
	lock_state().stdout_closed
}

fn act(action: Action) {
	match action {
		Action::Quit(code) => quit(code),
		Action::Send(press) => {
			// No receiver is not an error: the command that owns Ctrl-C may be ending.
			let _ = PRESSES.send(press);
		}
		Action::Nothing => {}
	}
}

fn lock_state() -> std::sync::MutexGuard<'static, State> {
	STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Quits the CLI with `code`, after Ctrl-C or once stdout is closed.
fn quit(code: i32) -> ! {
	// A progress bar hides the cursor, which would stay hidden in the shell.
	let term = console::Term::stderr();
	if term.is_term() {
		let _ = term.show_cursor();
	}
	log::info!("Quit with exit code {code}");
	process::exit(code)
}

/// What the CLI is doing about Ctrl-C.
#[derive(Debug, PartialEq)]
struct State {
	/// How many commands are running that Ctrl-C cancels. While none is, Ctrl-C quits the CLI.
	catching: usize,
	/// How many commands are running that handle Ctrl-C themselves.
	owning: usize,
	/// How many of those are stopping after a Ctrl-C, which a further Ctrl-C does not wait for.
	stopping: usize,
	/// Whether a Ctrl-C has cancelled a command that has not ended yet.
	cancelling: bool,
	/// Whether stdout was closed.
	stdout_closed: bool,
}

/// What a Ctrl-C does.
#[derive(Debug, PartialEq)]
enum Action {
	/// Quit the CLI with this exit code.
	Quit(i32),
	Send(Press),
	Nothing,
}

/// A Ctrl-C, as sent to the commands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Press {
	/// Whether a command handles it itself, so the one running it does not cancel it.
	owned: bool,
}

impl State {
	fn press(&mut self) -> Action {
		if self.catching == 0 || self.cancelling || self.stopping > 0 {
			return Action::Quit(EXIT_INTERRUPTED);
		}
		self.end_running_command()
	}

	fn close_stdout(&mut self) -> Action {
		if self.stdout_closed {
			return Action::Nothing;
		}
		self.stdout_closed = true;
		if self.catching == 0 {
			return Action::Quit(0);
		}
		self.end_running_command()
	}

	/// Cancels the running command, or has it stop if it handles Ctrl-C itself.
	fn end_running_command(&mut self) -> Action {
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
/// Cancelling drops `command` and fails with [`CliError::Interrupted`], or
/// [`CliError::StdoutClosed`] when a closed stdout cancelled it.
pub(crate) async fn cancellable<T>(
	command: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
	let mut catching = catch();
	tokio::select! {
		result = command => result,
		() = catching.cancelled() => Err(if stdout_closed() {
			CliError::StdoutClosed
		} else {
			CliError::Interrupted
		}
		.into()),
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

	/// Whether Ctrl-C was pressed since the last call, without waiting: for a command that blocks
	/// a thread, as the docs server does, and looks between its waits.
	pub(crate) fn was_pressed(&mut self) -> bool {
		match self.presses.try_recv() {
			Ok(_) | Err(TryRecvError::Lagged(_)) => true,
			// `PRESSES` is static, so its sender is never dropped.
			Err(TryRecvError::Empty | TryRecvError::Closed) => false,
		}
	}
}

impl Owning {
	/// Lets a further Ctrl-C quit the CLI while the command stops after one, rather than wait for
	/// it, until the returned [`Stopping`] is dropped, and says so.
	pub(crate) fn into_stopping(self, ui: &mut UI) -> Stopping {
		ui.print_muted_to_stderr("Press Ctrl-C again to quit.");
		lock_state().stopping += 1;
		// `self` is dropped after this, so the command counts as stopping before it no longer
		// counts as handling Ctrl-C.
		Stopping
	}
}

impl Drop for Owning {
	fn drop(&mut self) {
		let mut state = lock_state();
		state.owning = state.owning.saturating_sub(1);
	}
}

/// Held while a command stops after Ctrl-C: see [`Owning::into_stopping`].
pub(crate) struct Stopping;

impl Drop for Stopping {
	fn drop(&mut self) {
		let mut state = lock_state();
		state.stopping = state.stopping.saturating_sub(1);
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
			stopping: 0,
			cancelling: false,
			stdout_closed: false,
		}
	}

	#[test]
	fn ctrl_c_with_no_command_running_quits() {
		assert_eq!(state(0, 0).press(), Action::Quit(EXIT_INTERRUPTED));
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
		assert_eq!(state.press(), Action::Quit(EXIT_INTERRUPTED));
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

	#[test]
	fn ctrl_c_while_a_command_stops_after_one_quits() {
		let mut state = state(1, 1);
		state.press();
		state.stopping += 1;
		assert_eq!(state.press(), Action::Quit(EXIT_INTERRUPTED));
	}

	#[test]
	fn a_closed_stdout_with_no_command_running_quits_with_0() {
		let mut state = state(0, 0);
		assert_eq!(state.close_stdout(), Action::Quit(0));
		assert!(state.stdout_closed);
	}

	#[test]
	fn a_closed_stdout_cancels_a_running_command() {
		let mut state = state(1, 0);
		assert_eq!(state.close_stdout(), Action::Send(Press { owned: false }));
		assert!(state.cancelling);
	}

	#[test]
	fn a_closed_stdout_stops_a_command_that_handles_ctrl_c() {
		let mut state = state(1, 1);
		assert_eq!(state.close_stdout(), Action::Send(Press { owned: true }));
		assert!(!state.cancelling);
	}

	#[test]
	fn stdout_found_closed_again_while_the_command_stops_does_nothing() {
		let mut state = state(1, 1);
		state.close_stdout();
		state.stopping += 1;
		assert_eq!(state.close_stdout(), Action::Nothing);
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
	async fn a_command_that_handles_ctrl_c_can_look_for_it_without_waiting() {
		let _handler = HANDLER.lock().await;
		let catching = catch();
		let mut ctrl_c = own();
		assert!(!ctrl_c.was_pressed());

		on_ctrl_c();

		assert!(ctrl_c.was_pressed());
		// each Ctrl-C is seen once
		assert!(!ctrl_c.was_pressed());
		drop(ctrl_c);
		drop(catching);
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
