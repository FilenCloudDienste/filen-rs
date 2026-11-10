# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

Breaking changes are marked **Breaking**: scripts that depend on the old behaviour need a change.

### Changed

#### Output and exit codes

- **Breaking:** failures, warnings and log lines go to stderr; stdout holds only a command's output
- **Breaking:** an invalid command line exits with 2 (it exited with 1); `filen help exit-codes` lists every exit code
- **Breaking:** with `--json`, a failure is one line of JSON on stderr, `{"error": {"message", "kind", "exitCode"}}` (plus `reportUrl` for an unexpected error), instead of the `✘` text
- **Breaking:** the JSON of `ls`, `list-trash`, `list-favorites` and `list-recents` is an array of item objects (`name`, `type`, `size`, `modified`, `created`, `favorited`, `uuid`, `color`, and `path` where it applies), and that of `list-public-links` an array of link objects (`path`, `type`, `uuid`, `linkUuid`, `expiresAt`, `url`), instead of `{"directories": [...], "files": [...]}`; an empty listing prints `[]`
- **Breaking:** `search` prints one full path per line, and its `--json` is `{"results": [{"path", "type", "size", "uuid"}], "total", "complete"}`
- **Breaking:** `cat`, `head` and `tail` print the file's exact bytes, without adding a newline; with `--json` they print `{"text": ...}` and refuse a file that is not UTF-8
- `ls` and the search results print one item per line when stdout is not a terminal
- a closed stdout, as in `filen ls | head -1`, ends the command with exit code 0; a command writing a file's content to stdout (`cat`, `head`, `tail`, `download <file> -`) exits with 1 instead, as its reader did not get the whole file
- update notices, announcements and update check warnings are not printed under `--quiet` or `--json`, or when stdout is not a terminal; a failed update check is a warning and the command carries on; the update check of a command runs alongside it and is reported after its output
- a command's help, and the docs, show its whole description, not only its first paragraph
- listings sort directories first, then case-insensitively and naturally (`file2` before `file10`)
- dates are printed in UTC

#### Prompts and options

- **Breaking:** a question that cannot be asked (with `--no-input`, or without any terminal) fails the command with exit code 2 before it logs in, naming the option that answers it: scripts that ran `export-auth-config`, `empty-trash`, `rm -p` and other confirmed commands without a terminal now pass `--yes`
- **Breaking:** `export-auth-config` asks to confirm the risk and, if the file exists, to overwrite it (both default No), and refuses a path in a missing directory before it asks or logs in; `export-api-key` at a terminal asks before printing the key
- **Breaking:** `empty-trash` asks to confirm (`--yes` answers it), and prints how many items it deleted
- **Breaking:** the keyring login is scoped: debug builds use the service `filen-cli-rs-dev`, and a `--config-dir` other than the default keeps a login of its own (entry `sdk-config@<config dir>`), so users of a custom `--config-dir` log in again there; the default config directory keeps its existing login
- **Breaking:** after `filen rclone`, everything goes to Rclone as it is (`filen rclone -v ls filen:` runs `rclone -v ls filen:`, `filen rclone --help` shows Rclone's help); the CLI's options go before `rclone`, and `--rclone-binary` right after it
- `--yes` answers every yes/no question, from confirmations to "Enable download?", "Keep me logged in?" and the update offer, but not what `sync` asks about held-back deletions; questions that ask for a value still need a terminal or the option that gives it
- with stdin redirected (`filen rm -p f < list`, piped commands), prompts are asked at the controlling terminal, as git and ssh do
- the CLI's options are accepted after the command (`filen ls --json`), except `--password` and `--two-factor-code`; a REPL line can add `--json`, `--quiet`, `-v` or `--yes` for its own command
- `--config-dir` is also `--data-dir`, reads `FILEN_CLI_CONFIG_DIR` (or the older `FILEN_CLI_DATA_DIR`), and is created if it is missing
- `--working-path` must be a directory on the drive
- `--help` and `-h` work after a command, and in the REPL
- the credential options show their environment variables (`FILEN_CLI_EMAIL`, `FILEN_CLI_PASSWORD`, `FILEN_CLI_2FA_CODE`) in the help, never their values

#### Login

- a missing or unreadable `--auth-config-path` fails the command instead of logging in another way; a corrupt or revoked one fails with a hint to export a new one
- on Linux and macOS, an auth config found in a default location that other users can read is refused (and one given with `--auth-config-path` is used with a warning): restrict it with `chmod 600`
- saved credentials that are corrupt or revoked print a warning and lead to the login prompt; being offline no longer forces a new login
- a keyring that cannot be read at login prints a warning that says why before the CLI logs in another way
- the login prompt asks again after a wrong email, password or two-factor code, up to 3 times; the two-factor prompt also takes the recovery key
- a wrong two-factor code given with the option or environment variable is asked again at a terminal
- an email without a password (or the reverse) fails with a plain message instead of an unexpected error
- `logout` also offers to delete the auth config given with `--auth-config-path` and the local cache of the drive; in the REPL it exits only when something was deleted
- when the keyring cannot keep the login, the CLI says why and offers to save an unencrypted auth config in the config directory

#### Files

- absolute drive paths are normalized: `/a/../b`, `/a/./b`, `/a//b` and `/a/b/` resolve like their relative forms
- `cp` copies under a new name when the destination is not an existing directory, streams and copies in parallel, continues past items that fail and lists them, shows progress, refuses to overwrite an existing item, and fails early when the storage left is too small
- `mv` and `cp` refuse a destination that differs only in case from an existing item, and `mv` to a path ending in `/` needs an existing directory
- `mkdir` on an existing directory (`/` included) says it already exists instead of reporting it created, and fails on a file
- `ls -l` shows sizes, modification dates (creation dates of directories), colors and `(*)` for favorites; `ls <file>` shows that file
- `list-recents` keeps the server's order
- `stat` on a directory shows its color
- `head -n` and `tail -n` download only what they print
- `trash-restore` and `trash-delete` take items by name or UUID (several, or `--all`); the pickers show size and date, and an ambiguous name lists its candidates
- `search` waits at most 30 seconds for the search index and then prints what it has, with a warning, instead of failing
- `list-public-links` shows a path, expiry and URL per link, sorted by path; `shared-by-me` lists items by their drive path
- `public-link` asks for the settings before it creates a link (Esc creates nothing), offers "Enable download?" for files, keeps a link's password when the answer is empty (`-` removes it), and without a terminal creates the link with the default settings or prints an existing one; its table shows the link's UUID, whether a file link has a password, and when the link expires; the URL is printed
- `export-notes` exports straight into a missing or empty directory (created only when there are notes), and into a new `filen-notes-export-<timestamp>` subdirectory otherwise

#### Transfers

- `upload` and `download` take several sources (`<SOURCE> [PATH]...`, the last path is the destination) and the destination may be the new path of the item; missing directories on the way are created
- a file uploaded onto an existing file replaces it as a new version
- `download` of a single file replaces a local file of the same size when the drive has no hash for it, and refuses an item whose name is not a plain file name
- `upload` of a symlink keeps the link's own name
- the progress line shows the speed and the time left; the summary shows the files, size, time and average speed, and how many items were skipped
- with `--json`, `upload` and `download` print one result object (`destination`, `files`, `bytes`, `skipped`, `resumed_bytes`, `failed`)
- Ctrl-C reports what was transferred and what failed before exiting with 130, and also cancels a directory upload during its local scan

#### Mount and serve

- `mount` and `serve` run through one Rclone process configured in memory: no `rclone.conf` is written (a leftover one is deleted), and on Ctrl-C uploads still in the cache get up to 30 seconds to finish
- **Breaking:** `serve` listens on 127.0.0.1 and a port per server type unless `--addr` or `--host` says otherwise, and uses random credentials (printed when it starts, even with `--quiet`) unless `--user` and `--password` are both given
- **Breaking:** `serve` refuses an address without an IP (`:<port>`), a host name, or an IPv6 address without brackets; use `0.0.0.0:<port>` to listen on every network interface
- **Breaking:** `serve <type>` with an unknown type is a usage error (exit code 2), and `serve restic --read-only` is refused
- **Breaking:** `serve --root <PATH>` takes the directory by its path from the root of the drive, not from the working directory
- several `mount` and `serve` commands can run at the same time, each on a cache of its own (`cache`, then `cache-2` up to `cache-8`), instead of the latest one stopping the others
- a refused mount or server prints the reason (for example "port 1900 is in use") instead of an unexpected error, with what to install when FUSE is missing
- `mount` on macOS works without macFUSE, through Rclone's NFS mount
- `filen webdav`, `filen s3` and `filen webdav-proxy`, the older CLI's commands, say what replaced them and exit with 2
- with `--json`, Rclone notices go to stderr
- an interrupted `filen rclone` (Ctrl-C, SIGTERM or SIGHUP) asks Rclone to stop and gives it up to 30 seconds, so a mount it runs is unmounted and partial files are removed, and exits with Rclone's exit code; a second one kills Rclone at once. Under systemd, use `KillMode=mixed`

#### Shell, logs and updates

- **Breaking:** under `nohup`, closing the terminal now stops the command, as SIGHUP interrupts it like Ctrl-C; run it with `setsid` or under a service manager to keep it running
- the log files are created readable by their owner only, keep printed output, prompt answers and secrets out, start `latest.log` empty, expire moved-aside files and cap their total size; the CLI runs on when they cannot be written
- commands piped into the CLI run one line at a time; Ctrl-D at an empty prompt leaves the REPL, Ctrl-C clears the typed line, and a malformed line no longer ends the REPL
- Esc at an empty REPL prompt leaves the REPL
- every command can be interrupted with Ctrl-C, SIGTERM or SIGHUP (exit code 130; `filen rclone` exits with Rclone's own exit code instead); a second one quits while `mount`, `serve` or `sync` stops
- `cd` is refused outside the REPL
- REPL tab completion works on quoted paths and paths with escaped spaces, and quotes what it completes; on Windows, only double quotes group words and backslashes are kept, as in paths; it lists a directory once per prompt instead of on every keystroke
- `view-html-docs` stops on one Ctrl-C, loads no fonts from elsewhere, and listens on a port the system picks, so another program cannot take it first
- the update is staged next to the binary before it replaces it, offered only for a newer version, shows the release notes first, and restarts the REPL on the new version; in the Docker image, a newer release is announced with the `docker pull` command instead of installed
- the CLI releases the drive locks it took before it exits, waiting at most 5 seconds, so the next command does not wait for them to expire

### Added

#### Sync

- `sync` keeps local directories in sync with directories in the drive, continuously until Ctrl-C, or once with `--once` (`--continuous` is accepted and does nothing)
- sync pairs as `<local>:<remote>` or `<local>:<mode>:<remote>` (positional or with `-p`/`--pair`), by alias, or from a sync pairs file: `syncPairs.yaml` in the config directory, or the older CLI's `syncPairs.json` (also from `~/.filen-cli`); modes `twoWay`, `localToCloud`, `cloudToLocal`, `localBackup`, `cloudBackup` and their short forms; the `ignore`, `excludeDotFiles` and `alias` keys
- a missing local or remote root is created after asking (`--yes` answers it)
- the sync state the older CLI kept for a pair is imported the first time the pair syncs
- the delete guard holds back mass deletions (`--approve-mass-deletions`), conflicts are reported and resolved with `--keep-local`, `--keep-remote` or `--keep-both`, and at a terminal `sync` asks about both once a pass ends; replaced and deleted local files are kept in `.filen-sync-trash` for 30 days
- in `localBackup` and `cloudBackup`, a path where one side holds a file and the other a directory is a conflict that only `--keep-both`, or `--keep-remote` over a local file, resolves: `sync` asks only about those, and reports another `--keep-*` as a failure for that path (`resolve_failed` with `--json`)
- `sync --dry-run` prints what a pass would do; `--debounce-ms` (1 to 60000, default 800) and `--safety-net-secs` tune a continuous sync
- `sync --json` prints one JSON object per event; a status line shows the transfers at a terminal; `--quiet` prints only what needs attention
- `sync --once` exits with 0 when every pair is clean and 1 otherwise; a continuous sync exits with 1 after Ctrl-C if a pair failed
- `list-sync-pairs`, `remove-sync-pair`, `set-sync-mode`, `pause-sync-pair`, `resume-sync-pair`, `list-sync-conflicts`, `resolve-sync-conflict` and `retry-sync`

#### Files

- `write` and `touch`, `pwd`, `open` and `edit` (in `$VISUAL`/`$EDITOR` or the default app, uploaded as a new version), `view` and `drive` (the web app URL, opened in the browser)
- `versions` to list, restore and delete a file's versions, `color` to set a directory's color, `tree`, `du`, and `ls -R`
- `uuid:<UUID>` in place of any path, several items for `rm`, `cat`, `favorite`, `unfavorite`, `mv` and `cp`, and wildcards (`*`, `?`, `[...]`) in the last segment of a drive path (`filen help cloud-paths`)
- `search --limit` and `--type file|dir`
- `list-trash -l`
- `public-link --expires`, `--password`/`--no-password`, `--download`/`--no-download`, `--show` and `--delete`

#### Transfers

- `upload -` uploads stdin, and `download <file> -` writes to stdout
- `download --resume` continues an unfinished download, checked against the drive's hash
- `upload --resume` continues an unfinished upload of a file of more than 7 MiB, for about a day
- `download` of public link URLs, without logging in when every source is a link (`--link-password` or `FILEN_CLI_LINK_PASSWORD` for a protected link)
- `upload --skip-existing`, `--update` and `--no-follow-symlinks`, `download --no-clobber`, and `--include`/`--exclude` for both
- `-v` lists each transferred file
- `upload` expands wildcards such as `*.jpg` in its local paths, as a Unix shell does, on Windows and in the REPL and piped commands

#### Account, login, sharing and notes

- `whoami`, `statfs`, `account` (with `versioning`, `login-alerts` and `delete-versions`), `events`, `2fa enable`/`disable`, `change-password`, `change-email` and `export-master-keys`
- `events --limit <n>` pages back through older events, at most 10 requests; a second holding more events than one request returns may be listed in part, with a warning naming it, and paging stops with a warning at a page holding only events that could not be read
- `--profile <NAME>` (or `FILEN_CLI_PROFILE`) and `profiles`, a login, cache and logs of their own per profile; profile names that differ only in case are one profile, and names Windows reserves for devices (`CON`, `NUL`, `COM1`, ...) are refused
- `export-auth-config [PATH]` takes a file or directory
- `FILEN_EMAIL`, `FILEN_PASSWORD` and `FILEN_2FA_CODE`, the older CLI's variables, are read when nothing else gives credentials; `FILEN_CLI_2FA_SECRET` makes the two-factor code from a TOTP secret
- the login the older CLI saved is imported before the login prompt, and a hint is printed once about the older CLI's other files
- `contacts`, `contact-request`, `contact-accept`, `contact-deny`, `contact-cancel`, `contact-remove`, `contact-block` and `contact-unblock`
- `share`, `shared-by-me`, `unshare` (asking first when it would stop sharing with everyone), `shared-with-me` (with `--from <email>`, only what that account shared), `leave-share` and `download-shared`
- `notes`, `note-cat`, `note-new`, `note-edit`, `note-rm` and `import-notes`

#### Mount and serve

- `serve restic` and `serve nfs` (NFS has no authentication and listens on 127.0.0.1 only)
- `serve --cert` and `--key` to serve WebDAV, S3, HTTP and restic over HTTPS
- `serve --self-signed` to serve them over HTTPS with a self-signed certificate made once in the config directory, printing its SHA-256 fingerprint
- `serve` with several types at once, `--host`, `--mount` and `--mount-point`; `--password-file` and `FILEN_CLI_SERVE_PASSWORD`
- `mount --read-only` and `mount --root`
- `--json` for `mount` and `serve`, and a status line of the uploads and the cache at a terminal
- `rclone-info`, `clear-rclone-cache` and `--rclone-binary`

#### Shell

- `--yes` and `--no-input` to run without prompts
- `--log-file` and `--no-log-file`
- `completions <bash|zsh|fish|powershell|elvish>`, and `lcd`, `lpwd` and `lls` in the REPL
- the REPL history is kept across runs in `repl_history` in the config directory, without lines that hold a secret
- `version`, and `install <latest|vX.Y.Z>` to update, reinstall or downgrade
- help topics on the config directory, the log files, JSON output, exit codes, shell completions and migrating from the older CLI (`filen help migration`)

### Removed

- `mount` and `serve`: `--transfers` and additional Rclone arguments (`filen rclone` takes any Rclone option)

### Fixed

- `edit` reports that it kept the changes it could not upload as a failure, not as an unexpected error asking for a bug report
- a failed write of a file's content to stdout (`cat`, `head`, `tail`, `download -`) is a failure, not an unexpected error asking for a bug report
- prompt login with two-factor authentication masks the code and keeps the spaces around a prompted password
- `mv x /new/` and `mkdir -r /x/y/` no longer panic
- `cp` no longer silently replaces or merges into an item of the same name
- a directory download no longer counts a local file it kept as downloaded, and its progress reaches 100%
- `cat` of a large file no longer asks when no one can answer
- `head` and `tail` no longer ask before printing from a large file
- viewing a file public link no longer replaces it
- `logout` offers to delete a keyring entry the keyring cannot decode, so it can be removed, and exits with an error naming both causes when it can neither read nor delete it, instead of reporting nothing deleted
- when `public-link` creates a link but fails to save its settings, it deletes the link if those settings asked for a password, an expiry or downloads off, so the item is not left public without them; otherwise, or if the delete fails too, the error says the link is public without the settings and how to change or delete it
- `export-notes` replaces control characters in note titles and prefixes `_` to titles Windows reserves for a device, such as `CON`, `NUL`, `COM0` or `LPT¹`, also with spaces before the extension
- `filen rclone` exits with 128 plus the signal when a signal ended Rclone, instead of 0
- a terminal error while reading a text or password prompt is a `terminal_input` failure, not an unexpected one with a link to report a bug
- an auth config is written to a temporary file next to it and renamed over it, so an interrupted write no longer leaves a truncated one; a symlinked auth config keeps its link
- `mount` and `serve --cache-size` accept only `n`, `nB` and `nU`, `nUi`, `nUB` or `nUiB` with a unit `K`, `M`, `G` or `T`, and refuse a malformed suffix such as `10iB` (read as 10 bytes) or `10MBB` (read as 10 MiB)
- a sync pair with an empty local or remote path, as in `:/docs` or `/home/me/docs:`, is refused instead of syncing the working directory or the remote working path
- a sync pairs file that gives an alias `sync` cannot select, one with `:` or a path separator or an empty one, is refused
- on Windows, `download` and `download-shared` print `C:\...` paths instead of `\\?\C:\...`, where the path means the same without the prefix; they still write into an existing destination through its `\\?\` path, so a downloaded name ending in a dot or a space keeps it there
- `note-edit` reads the note again before saving and, when it changed while it was being edited, saves nothing and keeps the edit in its temporary file, whose path it names, instead of overwriting the change; it keeps and names the edit too when it cannot save it for another reason, such as the note having been deleted meanwhile
- an update installed under a new name, next to a binary whose name carries its version, stays installed when the old binary cannot be deleted, with a warning to delete it, instead of failing the command
- `view-html-docs` answers a request for an unknown section with 404 Not Found instead of 200 OK
- a failed write to stdout, as on a full disk, ends the command with a message and exit code 1 instead of a crash; in the REPL it fails only that command

### Security

- the updater checks a downloaded update against the SHA-256 digest its release lists, and does not install one that differs, whose size differs, or whose release lists a digest in another algorithm; an HTTP error page is no longer installed as the update
- the log files leave out secrets, and are readable by their owner only
- REPL history leaves out lines that hold a password or secret

## 0.2.9 - 2026-10-02

### Security

- authenticate rclone process with a random password to prevent unauthorized access to the Rclone RC API

## 0.2.8 - 2026-09-29

### Added

- autocompletion in interactive mode, including remote paths
- `search` command to globally search for files and directories in the drive (interactive and non-interactive modes)
- `upload` and `download` commands to transfer files and directories to and from the drive
- `trash-restore` and `trash-delete` commands to restore or permanently delete files and directories from the trash
- `ls --long` (`ls -l`) to list items one per line with human-readable file sizes
- `export-notes` command to export all notes to a local directory
- `list-favorites` and `list-recents` commands
- `--working-path` global option
- `list-public-links` and `public-link` commands to manage public links for files and directories

### Changed

- improved `mv` to include renaming and handle more special cases
- `stat <directory>` and `stat /` now show total size and amount of files and directories

## 0.2.7 - 2026-06-19

### Added

- `view-html-docs` command to view the in-app docs locally in a browser rendered as HTML

### Fixed

- bug where prompt login with 2FA enabled failed with error message instead of displaying a prompt for the 2FA code

## 0.2.6 - 2026-06-04

### Changed

- managed Rclone now uses upstream Rclone (instead of filen-rclone fork), which is more maintained

## 0.2.5 - 2026-01-29

### Added

- rclone commands (`mount`, `serve`) are more customizable via `--cache-size` and `--transfers` flags
  as well as the option to take any number of custom Rclone options
- convenient install script on Linux and macOS
- convenient script to temporarily install the CLI and execute `export-api-key` for use with Rclone

### Changed

- unified `webdav`, `ftp`, `sftp`, `http-server`, `s3` commands as `serve` command

### Fixed

- `cd` command: check if directory is valid #7
- `serve` command: don't automatically add IP to `--addr` option

## 0.2.4 - 2026-01-20

### Added

- `webdav`, `ftp`, `sftp`, `http-server`, `s3` shorthand commands to serve the drive via managed Rclone

## 0.2.3 - 2026-01-12

### Added

- updater can display important announcements fetched from the release repo
- Docker builds
- `export-api-key` command for use with unmanaged Rclone

## 0.2.2 - 2025-12-23

### Added

- accept two-factor code in cli args and env variables
- display global options help in docs
- `--force-update-check` flag to ignore recent update checks
- `mkdir -r` flag to recursively create parent directories
- `rclone` subcommand that executes commands using an automatically downloaded
  and managed installation of filen-rclone
- `--json` global flag to output machine-readable JSON where applicable
- fallback to exporting an auth config when system keyring fails,
  `logout` by deleting credentials from keyring or auth configs

### Fixed

- bug where command history didn't work in interactive mode
- adhere to `NO_COLOR` environment variable

## 0.2.1 - 2025-12-19

### Added

- update checker: don't check for updates for some time after checking
- generate styled in-app docs and markdown docs (at docs.filen.io) from a single code-adjacent source

## 0.2.0 - 2025-11-19

- initial release
