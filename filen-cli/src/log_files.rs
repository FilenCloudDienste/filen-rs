use std::{
	ffi::OsString,
	fs::{self, File, OpenOptions},
	io::{self, Write},
	path::{Path, PathBuf},
	sync::{Mutex, PoisonError},
	time::{Duration, SystemTime},
};

use chrono::{DateTime, Local, NaiveDate};
use log::{LevelFilter, Log, Metadata, Record};

/// The file in the log directory that holds the latest run, from its start.
pub(crate) const LATEST: &str = "latest.log";

/// How each line is timestamped: ftail's default, in local time.
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// The CLI's log files: `latest.log`, and a `<date>.log` per day next to it.
///
/// Writes what ftail's `single_file` and `daily_file` channels wrote, line for line, but formats
/// each record once and writes it to both files under one lock. ftail also listed and stat'ed the
/// whole directory on every record to delete expired daily files; this does it on the first record
/// and on the first record of each later day.
pub(crate) struct LogFiles {
	dir: PathBuf,
	level: LevelFilter,
	/// A file is moved aside before a write once it holds more than this, in bytes.
	max_file_size: u64,
	/// A `.log` file in `dir` is deleted once more than this many whole days have passed since it
	/// was last written, checked on the first record and on the first record of each later day.
	retention_days: u64,
	files: Mutex<Files>,
}

struct Files {
	latest: LogFile,
	daily: LogFile,
	/// The day `daily` is for.
	day: NaiveDate,
	/// Whether expired files are still to be deleted, which waits for a record so that
	/// `latest.log` is written to first: a `latest.log` left by a run days ago would expire too.
	prune_due: bool,
}

struct LogFile {
	path: PathBuf,
	file: File,
}

impl LogFiles {
	pub(crate) fn open(
		dir: &Path,
		level: LevelFilter,
		max_file_size: u64,
		retention_days: u64,
	) -> io::Result<Self> {
		let path = dir.join(LATEST);
		// Overwrites from the start without truncating, as ftail's `single_file` did, so a run
		// that logs less than the one before it leaves the end of that run's lines behind.
		let latest = LogFile {
			file: OpenOptions::new()
				.create(true)
				.write(true)
				.truncate(false)
				.open(&path)?,
			path,
		};
		let day = Local::now().date_naive();
		Ok(Self {
			dir: dir.to_owned(),
			level,
			max_file_size,
			retention_days,
			files: Mutex::new(Files {
				latest,
				daily: LogFile::open_daily(dir, day)?,
				day,
				prune_due: true,
			}),
		})
	}

	fn write_daily(&self, files: &mut Files, now: NaiveDate, line: &str) -> io::Result<()> {
		// A record stamped just before midnight that takes the lock after one stamped just after it
		// goes to the newer day's file, as with ftail, rather than switching back.
		if now > files.day {
			files.daily = LogFile::open_daily(&self.dir, now)?;
			files.day = now;
			files.prune_due = true;
		}
		if files.prune_due {
			files.prune_due = false;
			self.delete_expired();
		}
		files.daily.write(line, self.max_file_size)
	}

	/// Deletes every `.log` file in `dir` last written more than `retention_days` ago. One that
	/// cannot be read or deleted is left for the next pass.
	fn delete_expired(&self) {
		let Ok(entries) = fs::read_dir(&self.dir) else {
			return;
		};
		let now = SystemTime::now();
		for entry in entries.flatten() {
			let path = entry.path();
			if path.extension().is_none_or(|ext| ext != "log") {
				continue;
			}
			let age = entry
				.metadata()
				.and_then(|metadata| metadata.modified())
				.ok()
				.and_then(|modified| now.duration_since(modified).ok())
				.unwrap_or(Duration::ZERO);
			if age.as_secs() / SECONDS_PER_DAY > self.retention_days {
				let _ = fs::remove_file(&path);
			}
		}
	}
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
		let line = format!(
			"{} {} {} {}\n",
			now.format(DATETIME_FORMAT),
			record.level(),
			record.target(),
			record.args()
		);
		let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
		// A logger has nowhere to report its own failure, so a line that cannot be written to a
		// file is dropped from it (ftail panicked instead).
		let _ = files.latest.write(&line, self.max_file_size);
		let _ = self.write_daily(&mut files, now.date_naive(), &line);
	}
}

impl LogFile {
	fn open_daily(dir: &Path, day: NaiveDate) -> io::Result<Self> {
		let path = dir.join(format!("{}.log", day.format("%Y-%m-%d")));
		Ok(Self {
			file: OpenOptions::new().create(true).append(true).open(&path)?,
			path,
		})
	}

	/// Writes `line`, first moving the file aside and starting a new one if it holds more than
	/// `max_file_size` bytes. The size is read from the file each time because another process
	/// may be appending to the same file. A file that cannot be moved aside is written past the
	/// cap rather than losing the line.
	fn write(&mut self, line: &str, max_file_size: u64) -> io::Result<()> {
		if self.file.metadata()?.len() > max_file_size {
			let _ = self.rotate(max_file_size);
		}
		self.file.write_all(line.as_bytes())
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
		self.file = OpenOptions::new()
			.create(true)
			.append(true)
			.open(&self.path)?;
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
			match OpenOptions::new()
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
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, 1024, 3).unwrap();
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
		let files = LogFiles::open(dir.path(), LevelFilter::Info, 1024, 3).unwrap();
		files.log(
			&Record::builder()
				.level(Level::Debug)
				.args(format_args!("hidden"))
				.build(),
		);

		assert_eq!(read(&dir, LATEST), "");
	}

	#[test]
	fn a_file_moves_to_the_next_free_old_name_once_it_holds_more_than_the_cap() {
		let dir = TempDir::new().unwrap();
		// Exactly one line, so the second line is written at the cap and the third past it.
		let cap = u64::try_from("2026-01-01 00:00:00 INFO target a\n".len()).unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, cap, 3).unwrap();
		for message in ["a", "b", "c", "d", "e"] {
			log_line(&files, message);
		}

		assert_eq!(messages(&read(&dir, "latest.log.old")), ["a", "b"]);
		assert_eq!(messages(&read(&dir, "latest.log.old1")), ["c", "d"]);
		assert_eq!(messages(&read(&dir, LATEST)), ["e"]);
	}

	#[test]
	fn loggers_sharing_a_directory_move_the_daily_file_aside_without_losing_a_line() {
		let dir = TempDir::new().unwrap();
		// Each logger stands in for a CLI process: its own lock and its own open files.
		let loggers = (0..8)
			.map(|_| LogFiles::open(dir.path(), LevelFilter::Debug, 512, 3).unwrap())
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
	fn the_first_record_deletes_expired_log_files_but_keeps_latest_and_moved_aside_ones() {
		let dir = TempDir::new().unwrap();
		for (name, age) in [
			(LATEST, 10),
			("2020-01-01.log", 10),
			("2020-01-05.log", 3),
			("2020-01-01.log.old", 10),
		] {
			File::create(dir.path().join(name))
				.unwrap()
				.set_times(days_ago(age))
				.unwrap();
		}
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, 1024, 3).unwrap();
		assert!(dir.path().join("2020-01-01.log").exists());
		log_line(&files, "hello");

		assert!(!dir.path().join("2020-01-01.log").exists());
		assert!(dir.path().join("2020-01-05.log").exists());
		assert!(dir.path().join("2020-01-01.log.old").exists());
		assert_eq!(messages(&read(&dir, LATEST)), ["hello"]);
	}

	#[test]
	fn the_first_record_of_a_later_day_starts_its_file_and_deletes_expired_ones() {
		let dir = TempDir::new().unwrap();
		let files = LogFiles::open(dir.path(), LevelFilter::Debug, 1024, 3).unwrap();
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
