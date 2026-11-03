use std::{
	ffi::{OsStr, OsString},
	fs::{self, File, OpenOptions},
	io::{self, Write},
	path::{Path, PathBuf},
	sync::{Mutex, PoisonError},
	time::{Duration, SystemTime},
};
#[cfg(unix)]
use std::{
	fs::Permissions,
	os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

use chrono::{DateTime, Local, NaiveDate};
use log::{LevelFilter, Log, Metadata, Record};

/// The file in the log directory that holds the latest run, from its start.
pub(crate) const LATEST: &str = "latest.log";

/// How each line is timestamped: ftail's default, in local time.
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// The mode of each log file on unix: only its owner can read or write it, as the log files can
/// hold paths, file names and error details from the account.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// The mode of the log directory on unix: only its owner can list or enter it.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Options that create a file with [`PRIVATE_FILE_MODE`] on unix.
pub(crate) fn private_file_options() -> OpenOptions {
	let mut options = OpenOptions::new();
	#[cfg(unix)]
	options.mode(PRIVATE_FILE_MODE);
	options
}

/// Opens `path` with `options`, then sets [`PRIVATE_FILE_MODE`] on it on unix: the mode in
/// `options` applies only to a file the open creates, and a log file that already exists may have
/// been created with the default mode by an older CLI.
pub(crate) fn open_private(options: &OpenOptions, path: &Path) -> io::Result<File> {
	let file = options.open(path)?;
	#[cfg(unix)]
	file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
	Ok(file)
}

/// The CLI's log files: `latest.log`, and a `<date>.log` per day next to it.
///
/// Writes what ftail's `single_file` and `daily_file` channels wrote, line for line, but formats
/// each record once and writes it to both files under one lock. ftail also listed and stat'ed the
/// whole directory on every record to delete expired daily files; this prunes the directory
/// ([`LogLimits`]) after the first record, the first record of each later day and each record that
/// started a new file.
pub(crate) struct LogFiles {
	dir: PathBuf,
	level: LevelFilter,
	limits: LogLimits,
	files: Mutex<Files>,
}

/// How large the log files grow and how long they are kept.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LogLimits {
	/// A file is moved aside before a write once it holds more than this, in bytes.
	pub(crate) max_file_size: u64,
	/// A log file in the directory, moved-aside ones included, is deleted once more than this many
	/// whole days have passed since it was last written.
	pub(crate) retention_days: u64,
	/// Once the log files in the directory hold more than this together, in bytes, the oldest are
	/// deleted until they do not. The two files being written count but are never deleted.
	pub(crate) max_total_size: u64,
}

struct Files {
	latest: LogFile,
	daily: LogFile,
	/// The day `daily` is for.
	day: NaiveDate,
	/// Whether the directory is pruned after the next record.
	prune_due: bool,
}

struct LogFile {
	path: PathBuf,
	file: File,
}

/// Closes the log directory `dir` to other users on unix: older CLIs created the log files with
/// the default mode, and the moved-aside and earlier daily ones are never opened again to set
/// theirs.
pub(crate) fn close_to_others(dir: &Path) -> io::Result<()> {
	#[cfg(unix)]
	fs::set_permissions(dir, Permissions::from_mode(PRIVATE_DIR_MODE))?;
	#[cfg(not(unix))]
	let _ = dir;
	Ok(())
}

impl LogFiles {
	/// Opens the log files in `dir`, which [`close_to_others`] has closed to other users.
	pub(crate) fn open(dir: &Path, level: LevelFilter, limits: LogLimits) -> io::Result<Self> {
		let latest = LogFile::open_latest(dir)?;
		let day = Local::now().date_naive();
		Ok(Self {
			dir: dir.to_owned(),
			level,
			limits,
			files: Mutex::new(Files {
				latest,
				daily: LogFile::open_daily(dir, day)?,
				day,
				prune_due: true,
			}),
		})
	}

	/// Deletes the log files in `dir` ([`is_log_file_name`]) last written more than
	/// `retention_days` ago, then the oldest of the others while they hold more than
	/// `max_total_size` together. The two files in `files` are never deleted, as this logger writes
	/// to them however old they look: on Windows a directory listing can report the time a file was
	/// last written before it was opened. One that cannot be read or deleted is left for the next
	/// pass, and one another process deleted first is not an error.
	fn prune(&self, files: &Files) {
		let Ok(entries) = fs::read_dir(&self.dir) else {
			return;
		};
		let now = SystemTime::now();
		let mut total = 0_u64;
		let mut deletable = Vec::new();
		for entry in entries.flatten() {
			if !is_log_file_name(&entry.file_name()) {
				continue;
			}
			let Ok(metadata) = entry.metadata() else {
				continue;
			};
			let path = entry.path();
			let size = metadata.len();
			if path == files.latest.path || path == files.daily.path {
				total = total.saturating_add(size);
				continue;
			}
			// A file whose age cannot be told counts as just written.
			let modified = metadata.modified().unwrap_or(now);
			let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
			if age.as_secs() / SECONDS_PER_DAY > self.limits.retention_days && remove(&path) {
				continue;
			}
			total = total.saturating_add(size);
			deletable.push((modified, size, path));
		}
		deletable.sort_unstable();
		for (_, size, path) in deletable {
			if total <= self.limits.max_total_size {
				break;
			}
			if remove(&path) {
				total = total.saturating_sub(size);
			}
		}
	}
}

/// Deletes the file at `path`; returns whether it is gone, deleted by another process included.
fn remove(path: &Path) -> bool {
	match fs::remove_file(path) {
		Ok(()) => true,
		Err(e) => e.kind() == io::ErrorKind::NotFound,
	}
}

/// Whether `name` is the name of a log file: `<name>.log`, or one moved aside from it,
/// `<name>.log.old`, `<name>.log.old1`, `<name>.log.old2`, ...
fn is_log_file_name(name: &OsStr) -> bool {
	let Some(name) = name.to_str() else {
		return false;
	};
	name.ends_with(".log")
		|| name
			.rsplit_once(".log.old")
			.is_some_and(|(_, n)| n.bytes().all(|b| b.is_ascii_digit()))
}

impl Log for LogFiles {
	fn enabled(&self, metadata: &Metadata) -> bool {
		metadata.level() <= self.level
	}

	fn log(&self, record: &Record) {
		if self.enabled(record.metadata()) {
			self.write_record(Local::now(), record);
		}
	}

	// Every line is written straight to its file, so there is nothing buffered to flush.
	fn flush(&self) {}
}

impl LogFiles {
	fn write_record(&self, now: DateTime<Local>, record: &Record) {
		let line = format_line(now, record);
		let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
		let files = &mut *files;
		// A record stamped just before midnight that takes the lock after one stamped just after it
		// goes to the newer day's file, as with ftail, rather than switching back.
		let day = now.date_naive();
		if day > files.day {
			// The line still goes to `latest.log` when the new day's file cannot be opened.
			if let Ok(daily) = LogFile::open_daily(&self.dir, day) {
				files.daily = daily;
				files.day = day;
				files.prune_due = true;
			}
		}
		// A logger has nowhere to report its own failure, so a line that cannot be written to a
		// file is dropped from it (ftail panicked instead).
		for file in [&mut files.latest, &mut files.daily] {
			if file
				.write(&line, self.limits.max_file_size)
				.unwrap_or(false)
			{
				files.prune_due = true;
			}
		}
		if files.prune_due {
			files.prune_due = false;
			self.prune(files);
		}
	}
}

/// `record` as a line of a log file, logged at `now`.
fn format_line(now: DateTime<Local>, record: &Record) -> String {
	format!(
		"{} {} {} {}\n",
		now.format(DATETIME_FORMAT),
		record.level(),
		record.target(),
		record.args()
	)
}

/// The log file `--log-file` names, which the CLI writes instead of [`LogFiles`]. Each line is
/// appended to it, in the format of [`LogFiles`], and it is never moved aside, emptied or
/// deleted, as it is the user's.
pub(crate) struct ChosenLogFile {
	level: LevelFilter,
	file: Mutex<File>,
}

impl ChosenLogFile {
	/// Opens the file at `path` for appending, creating it with [`PRIVATE_FILE_MODE`] on unix if
	/// it does not exist. The mode of an existing file is left as it is.
	pub(crate) fn open(path: &Path, level: LevelFilter) -> io::Result<Self> {
		Ok(Self {
			level,
			file: Mutex::new(
				private_file_options()
					.create(true)
					.append(true)
					.open(path)?,
			),
		})
	}
}

impl Log for ChosenLogFile {
	fn enabled(&self, metadata: &Metadata) -> bool {
		metadata.level() <= self.level
	}

	fn log(&self, record: &Record) {
		if self.enabled(record.metadata()) {
			let line = format_line(Local::now(), record);
			let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
			// A logger has nowhere to report its own failure, so a line that cannot be written is
			// dropped.
			let _ = file.write_all(line.as_bytes());
		}
	}

	// Every line is written straight to the file, so there is nothing buffered to flush.
	fn flush(&self) {}
}

impl LogFile {
	/// Opens `latest.log` emptied, so that it holds this run's lines only, and for appending, so
	/// that the lines of another CLI process writing to it at the same time go after this one's
	/// rather than over them. The previous run's lines are still in its daily file.
	fn open_latest(dir: &Path) -> io::Result<Self> {
		let path = dir.join(LATEST);
		// Emptied by a separate open, since a file opened for appending cannot be truncated on
		// Windows.
		open_private(
			private_file_options()
				.create(true)
				.write(true)
				.truncate(true),
			&path,
		)?;
		Ok(Self {
			file: open_private(private_file_options().create(true).append(true), &path)?,
			path,
		})
	}

	fn open_daily(dir: &Path, day: NaiveDate) -> io::Result<Self> {
		let path = dir.join(format!("{}.log", day.format("%Y-%m-%d")));
		Ok(Self {
			file: open_private(private_file_options().create(true).append(true), &path)?,
			path,
		})
	}

	/// Writes `line`, first moving the file aside and starting a new one if it holds more than
	/// `max_file_size` bytes; returns whether it did. The size is read from the file each time
	/// because another process may be appending to the same file. A file that cannot be moved aside
	/// is written past the cap rather than losing the line.
	fn write(&mut self, line: &str, max_file_size: u64) -> io::Result<bool> {
		let full = self.file.metadata()?.len() > max_file_size;
		let rotated = full && self.rotate(max_file_size).is_ok();
		self.file.write_all(line.as_bytes())?;
		Ok(rotated)
	}

	/// Moves the file at `path` aside if it holds more than `max_file_size` bytes, then opens the
	/// file at `path`, creating it if needed. Another CLI process sharing the directory may have
	/// moved it aside already and started a new one, or none yet; that one is opened instead.
	fn rotate(&mut self, max_file_size: u64) -> io::Result<()> {
		match fs::metadata(&self.path) {
			Ok(metadata) if metadata.len() > max_file_size => self.move_aside()?,
			Ok(_) => {}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => return Err(e),
		}
		self.file = open_private(private_file_options().create(true).append(true), &self.path)?;
		Ok(())
	}

	/// Renames the file at `path` to the first free one of `<path>.old`, `<path>.old1`,
	/// `<path>.old2`, ...
	///
	/// The name is claimed by creating it empty first, so that two processes moving the file aside
	/// at once each get a name of their own: a rename onto a name that was free when checked could
	/// replace the file another process had just moved there. A file already moved away by another
	/// process is not an error.
	fn move_aside(&self) -> io::Result<()> {
		let claimed = self.claim_old_name()?;
		fs::rename(&self.path, &claimed).or_else(|e| {
			let _ = fs::remove_file(&claimed);
			match e.kind() {
				io::ErrorKind::NotFound => Ok(()),
				_ => Err(e),
			}
		})
	}

	fn claim_old_name(&self) -> io::Result<PathBuf> {
		for n in 0..=u64::MAX {
			let candidate = match n {
				0 => self.with_suffix(".old"),
				n => self.with_suffix(&format!(".old{n}")),
			};
			match private_file_options()
				.write(true)
				.create_new(true)
				.open(&candidate)
			{
				Ok(_) => return Ok(candidate),
				Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
				Err(e) => return Err(e),
			}
		}
		Err(io::ErrorKind::AlreadyExists.into())
	}

	fn with_suffix(&self, suffix: &str) -> PathBuf {
		let mut path = OsString::from(self.path.as_os_str());
		path.push(suffix);
		PathBuf::from(path)
	}
}

#[cfg(test)]
mod tests {
	use std::{fs::FileTimes, thread};

	use assert_fs::TempDir;
	use chrono::{NaiveDateTime, TimeDelta};
	use log::Level;

	use super::*;

	fn log_line(files: &LogFiles, message: &str) {
		files.log(
			&Record::builder()
				.level(Level::Info)
				.target("target")
				.args(format_args!("{message}"))
				.build(),
		);
	}

	fn log_line_at(files: &LogFiles, now: DateTime<Local>, message: &str) {
		files.write_record(
			now,
			&Record::builder()
				.level(Level::Info)
				.target("target")
				.args(format_args!("{message}"))
				.build(),
		);
	}

	/// The limits with `max_file_size`, three days' retention and no total size worth mentioning.
	fn limits(max_file_size: u64) -> LogLimits {
		LogLimits {
			max_file_size,
			retention_days: 3,
			max_total_size: u64::MAX,
		}
	}

	fn read(dir: &TempDir, name: &str) -> String {
		fs::read_to_string(dir.path().join(name)).unwrap()
	}

	/// The message of each line, without its timestamp, level and target.
	fn messages(text: &str) -> Vec<&str> {
		text.lines()
			.map(|line| line.rsplit_once(' ').unwrap().1)
			.collect()
	}

	fn daily_file_name(day: DateTime<Local>) -> String {
		format!("{}.log", day.format("%Y-%m-%d"))
	}

	fn days_ago(days: u64) -> FileTimes {
		FileTimes::new()
			.set_modified(SystemTime::now() - Duration::from_secs(days * SECONDS_PER_DAY))
	}

	#[test]
	fn a_record_goes_to_latest_and_to_the_daily_file_in_ftails_format() {
		let dir = TempDir::new().unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line(&files, "hello");

		let latest = read(&dir, LATEST);
		let [date, time, rest] = latest.splitn(3, ' ').collect::<Vec<_>>()[..] else {
			panic!("{latest:?}");
		};
		assert!(
			NaiveDateTime::parse_from_str(&format!("{date} {time}"), DATETIME_FORMAT).is_ok(),
			"{latest:?}"
		);
		assert_eq!(rest, "INFO target hello\n");
		// Named after the day in the line, whichever side of midnight the record fell.
		assert_eq!(read(&dir, &format!("{date}.log")), latest);
	}

	#[test]
	fn a_record_above_the_level_is_not_written() {
		let dir = TempDir::new().unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Info, limits(1024)).unwrap();
		files.log(
			&Record::builder()
				.level(Level::Debug)
				.args(format_args!("hidden"))
				.build(),
		);

		assert_eq!(read(&dir, LATEST), "");
	}

	#[test]
	fn a_chosen_log_file_is_appended_to_and_never_moved_aside() {
		let dir = TempDir::new().unwrap();
		let path = dir.path().join("cli.log");
		fs::write(&path, "an earlier line\n").unwrap();
		let file = ChosenLogFile::open(&path, LevelFilter::Info).unwrap();
		for message in ["a", "b"] {
			file.log(
				&Record::builder()
					.level(Level::Info)
					.target("target")
					.args(format_args!("{message}"))
					.build(),
			);
		}
		file.log(
			&Record::builder()
				.level(Level::Debug)
				.args(format_args!("hidden"))
				.build(),
		);

		assert_eq!(messages(&read(&dir, "cli.log")), ["line", "a", "b"]);
		let names = fs::read_dir(dir.path()).unwrap().count();
		assert_eq!(names, 1);
	}

	#[test]
	fn latest_holds_only_the_lines_of_the_last_run() {
		let dir = TempDir::new().unwrap();
		let first = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line(&first, "a longer line from the first run");
		drop(first);
		let second = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line(&second, "short");

		assert_eq!(messages(&read(&dir, LATEST)), ["short"]);
	}

	#[test]
	fn loggers_opening_latest_one_after_the_other_append_after_each_other() {
		let dir = TempDir::new().unwrap();
		let first = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line(&first, "first-1");
		let second = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line(&first, "first-2");
		log_line(&second, "second-1");

		let latest = read(&dir, LATEST);
		assert!(!latest.contains('\0'), "{latest:?}");
		assert_eq!(messages(&latest), ["first-2", "second-1"]);
	}

	#[test]
	fn a_file_moves_to_the_next_free_old_name_once_it_holds_more_than_the_cap() {
		let dir = TempDir::new().unwrap();
		// Exactly one line, so the second line is written at the cap and the third past it.
		let cap = u64::try_from("2026-01-01 00:00:00 INFO target a\n".len()).unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(cap)).unwrap();
		for message in ["a", "b", "c", "d", "e"] {
			log_line(&files, message);
		}

		assert_eq!(messages(&read(&dir, "latest.log.old")), ["a", "b"]);
		assert_eq!(messages(&read(&dir, "latest.log.old1")), ["c", "d"]);
		assert_eq!(messages(&read(&dir, LATEST)), ["e"]);
	}

	#[cfg(unix)]
	#[test]
	fn every_log_file_including_moved_aside_ones_is_readable_by_its_owner_only() {
		let dir = TempDir::new().unwrap();
		let cap = u64::try_from("2026-01-01 00:00:00 INFO target a\n".len()).unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(cap)).unwrap();
		let now = Local::now();
		for message in ["a", "b", "c"] {
			log_line_at(&files, now, message);
		}

		let mut modes = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| {
				let entry = entry.unwrap();
				let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
				(entry.file_name().into_string().unwrap(), mode)
			})
			.collect::<Vec<_>>();
		modes.sort_unstable();
		let today = daily_file_name(now);
		let mut expected = [
			(LATEST.to_owned(), 0o600),
			(format!("{LATEST}.old"), 0o600),
			(today.clone(), 0o600),
			(format!("{today}.old"), 0o600),
		];
		expected.sort_unstable();
		assert_eq!(modes, expected);
	}

	#[cfg(unix)]
	#[test]
	fn log_files_an_older_cli_left_readable_by_others_are_closed_to_them_on_open() {
		let dir = TempDir::new().unwrap();
		let today = daily_file_name(Local::now());
		let old = format!("{LATEST}.old");
		for name in [LATEST, today.as_str(), old.as_str()] {
			fs::write(dir.path().join(name), "secret\n").unwrap();
			fs::set_permissions(dir.path().join(name), Permissions::from_mode(0o644)).unwrap();
		}
		fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();

		close_to_others(dir.path()).unwrap();
		let _files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();

		let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode(dir.path()), 0o700);
		assert_eq!(mode(&dir.path().join(LATEST)), 0o600);
		assert_eq!(mode(&dir.path().join(&today)), 0o600);
	}

	#[test]
	fn loggers_sharing_a_directory_move_the_daily_file_aside_without_losing_a_line() {
		let dir = TempDir::new().unwrap();
		// Each logger stands in for a CLI process: its own lock and its own open files.
		let loggers = (0..8)
			.map(|_| LogFiles::open(dir.path(), LevelFilter::Debug, limits(512)).unwrap())
			.collect::<Vec<_>>();
		let now = Local::now();
		thread::scope(|scope| {
			for (i, logger) in loggers.iter().enumerate() {
				scope.spawn(move || {
					for n in 0..500 {
						log_line_at(logger, now, &format!("{i}-{n}"));
					}
				});
			}
		});

		let daily = daily_file_name(now);
		let mut written = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.filter(|name| name.starts_with(&daily))
			.flat_map(|name| {
				messages(&read(&dir, &name))
					.into_iter()
					.map(str::to_owned)
					.collect::<Vec<_>>()
			})
			.collect::<Vec<_>>();
		written.sort_unstable();
		let mut expected = (0..8)
			.flat_map(|i| (0..500).map(move |n| format!("{i}-{n}")))
			.collect::<Vec<_>>();
		expected.sort_unstable();
		assert_eq!(written, expected);
	}

	#[test]
	fn the_first_record_deletes_expired_log_files_moved_aside_ones_included() {
		let dir = TempDir::new().unwrap();
		for (name, age) in [
			(LATEST, 10),
			("2020-01-01.log", 10),
			("2020-01-05.log", 3),
			("2020-01-01.log.old", 10),
			("2020-01-01.log.old12", 10),
			("latest.log.old3", 10),
			("2020-01-05.log.old", 3),
			("notes.txt", 10),
			("2020-01-01.log.older", 10),
		] {
			File::create(dir.path().join(name))
				.unwrap()
				.set_times(days_ago(age))
				.unwrap();
		}
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		assert!(dir.path().join("2020-01-01.log").exists());
		log_line(&files, "hello");

		let mut left = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		left.sort_unstable();
		let mut expected = vec![
			"2020-01-01.log.older".to_owned(),
			"2020-01-05.log".to_owned(),
			"2020-01-05.log.old".to_owned(),
			LATEST.to_owned(),
			daily_file_name(Local::now()),
			"notes.txt".to_owned(),
		];
		expected.sort_unstable();
		assert_eq!(left, expected);
		assert_eq!(messages(&read(&dir, LATEST)), ["hello"]);
	}

	#[test]
	fn the_oldest_log_files_are_deleted_while_they_hold_more_than_the_total_cap() {
		let dir = TempDir::new().unwrap();
		for (name, age) in [
			("2020-01-05.log", 1),
			("2020-01-04.log.old", 2),
			("2020-01-03.log", 3),
			("2020-01-04.log", 2),
		] {
			let file = File::create(dir.path().join(name)).unwrap();
			file.set_len(100).unwrap();
			file.set_times(days_ago(age)).unwrap();
		}
		let files = LogFiles::open(
			dir.path(),
			LevelFilter::Debug,
			LogLimits {
				max_file_size: 1024,
				retention_days: 3,
				max_total_size: 250,
			},
		)
		.unwrap();
		log_line(&files, "hello");

		// 400 bytes and the line in each file being written: deleting the three oldest leaves 100
		// bytes and the two lines, two leave 200 and the two lines, more than 250.
		assert!(!dir.path().join("2020-01-03.log").exists());
		assert!(!dir.path().join("2020-01-04.log").exists());
		assert!(!dir.path().join("2020-01-04.log.old").exists());
		assert!(dir.path().join("2020-01-05.log").exists());
		assert_eq!(messages(&read(&dir, LATEST)), ["hello"]);
	}

	#[test]
	fn a_file_moved_aside_prunes_the_directory_to_the_total_cap() {
		let dir = TempDir::new().unwrap();
		let line = u64::try_from("2026-01-01 00:00:00 INFO target a\n".len()).unwrap();
		let files = LogFiles::open(
			dir.path(),
			LevelFilter::Debug,
			LogLimits {
				max_file_size: line,
				retention_days: 3,
				// The two files being written, a line each, and one moved-aside file of two lines.
				max_total_size: 5 * line,
			},
		)
		.unwrap();
		let now = Local::now();
		for message in ["a", "b", "c", "d", "e", "f", "g"] {
			log_line_at(&files, now, message);
		}

		let mut left = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		left.sort_unstable();
		assert_eq!(left.len(), 3, "{left:?}");
		assert_eq!(messages(&read(&dir, LATEST)), ["g"]);
	}

	#[test]
	fn log_file_names_are_told_apart_from_others() {
		for name in [
			"latest.log",
			"2020-01-01.log",
			"latest.log.old",
			"latest.log.old42",
		] {
			assert!(is_log_file_name(OsStr::new(name)), "{name}");
		}
		for name in [
			"latest.txt",
			"latest.log.older",
			"latest.log.old4a",
			"log",
			"rclone.conf",
		] {
			assert!(!is_log_file_name(OsStr::new(name)), "{name}");
		}
	}

	#[test]
	fn the_first_record_keeps_the_files_it_writes_to_however_old_they_look() {
		let dir = TempDir::new().unwrap();
		let today = Local::now();
		for name in [LATEST.to_owned(), daily_file_name(today)] {
			File::create(dir.path().join(name))
				.unwrap()
				.set_times(days_ago(10))
				.unwrap();
		}
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		log_line_at(&files, today, "hello");

		assert_eq!(messages(&read(&dir, LATEST)), ["hello"]);
		assert_eq!(messages(&read(&dir, &daily_file_name(today))), ["hello"]);
	}

	#[test]
	fn the_first_record_of_a_later_day_starts_its_file_and_deletes_expired_ones() {
		let dir = TempDir::new().unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, limits(1024)).unwrap();
		let today = Local::now();
		log_line_at(&files, today, "today");
		let expired = dir.path().join("2020-01-01.log");
		File::create(&expired)
			.unwrap()
			.set_times(days_ago(10))
			.unwrap();
		let tomorrow = today + TimeDelta::days(1);
		log_line_at(&files, tomorrow, "tomorrow");
		// Stamped before the last one but written after it, as when two threads race at midnight.
		log_line_at(&files, today, "late");

		assert!(!expired.exists());
		assert_eq!(messages(&read(&dir, &daily_file_name(today))), ["today"]);
		assert_eq!(
			messages(&read(&dir, &daily_file_name(tomorrow))),
			["tomorrow", "late"]
		);
		assert_eq!(messages(&read(&dir, LATEST)), ["today", "tomorrow", "late"]);
	}
}
