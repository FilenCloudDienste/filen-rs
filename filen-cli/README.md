# Filen CLI (public beta)

📩 [Releases](https://github.com/FilenCloudDienste/filen-cli-releases/releases) | 📖 [Documentation](https://docs.filen.io/docs/cli-rs/readme) | 📜 [Source](https://github.com/FilenCloudDienste/filen-rs/tree/main/filen-cli)

The Filen CLI gives you your Filen cloud drive on the command line: managing files and directories, transfers, syncing, network drives and servers, sharing, notes and your account.
Start it without specifying a command to enter interactive mode (a REPL with tab completion), or run one command at a time from a shell or a script.

> [!WARNING]
> **Public Beta:**
> This is the Rust rewrite of [`FilenCloudDienste/filen-cli`](https://github.com/FilenCloudDienste/filen-cli), which has been [sunsetted](https://github.com/FilenCloudDienste/filen-cli?tab=readme-ov-file#sunsetting-filen-cli).
> While it aims to fully replace it, it is currently in open beta: _there might be bugs_, and commands and their output may still change between releases.
> Coming from the older CLI? See [Migrating from the TypeScript CLI](#migrating-from-the-typescript-cli).

> [!NOTE]
> Please **report bugs** on our [issues page at `filen-rs`](https://github.com/FilenCloudDienste/filen-rs/issues?q=label%3Acli)! \
> **Feature requests** can be submitted on [features.filen.io](https://features.filen.io/?tags=cli).

## Installation and updates

💻 **Linux** and **macOS**:

```bash
curl -sL https://raw.githubusercontent.com/FilenCloudDienste/filen-rs/refs/heads/main/filen-cli/install.sh | bash
```

💻 **Windows**: Download the latest binaries from the [release page](https://github.com/FilenCloudDienste/filen-cli-releases/releases/latest).

🐋 Docker images are also available as [`filen/cli`](https://hub.docker.com/r/filen/cli/tags) (you need to specify a version instead of using `:latest`).

The CLI includes an automatic updater.

## Features

- **Files:** `ls`, `cd`, `stat`, `cat`, `head`, `tail`, `mkdir`, `mv`, `cp`, `rm`, `write`, `touch`, `edit`, `open`, `search`, `tree`, `du`, `versions`, favorites, recents and the trash
- **Transfers:** `upload` and `download` of files and directories, with progress, resuming, filters, stdin and stdout, and public link URLs
- **Sync:** `sync` keeps local directories and directories in your drive in sync, continuously or once, in two-way or one-way modes
- **Network drive and servers:** `mount` mounts your drive, and `serve` serves it over WebDAV, S3, FTP, SFTP, HTTP, restic or NFS, through a managed [Rclone](https://rclone.org/filen) that the CLI downloads and configures for you; `rclone` runs any Rclone command on your drive
- **Sharing:** public links, contacts, and items shared with and by you
- **Notes:** list, print, create, edit, import and export notes
- **Account:** storage, two-factor authentication, password and email, settings and events
- **Scripting:** `--json` output, `--yes` and `--no-input` for running without prompts, documented exit codes, and shell completions

```bash
filen                                         # interactive mode
filen ls -l /Documents                        # list a directory
filen upload ./photos /Photos                 # upload a directory
filen download /Documents/report.pdf .        # download a file
filen sync ~/Documents:/Documents             # keep a directory in sync until Ctrl-C
filen sync --once ~/Photos:localBackup:/Backup/Photos
filen mount                                   # mount your drive (/tmp/filen, or X: on Windows)
filen serve webdav                            # serve it over WebDAV on 127.0.0.1:1900
filen public-link /Documents/report.pdf       # create or edit a public link
filen --json ls /Documents                    # machine-readable output
```

## Documentation

You can find documentation from within the CLI using the `help` or `view-html-docs` subcommand and at [docs.filen.io](https://docs.filen.io/docs/cli-rs/readme/).

## Migrating from the TypeScript CLI

Your login and sync pairs carry over: the older CLI's saved login is imported the first time you log in, and `sync` reads its `syncPairs.json` and takes over the sync state it kept.
The main differences:

| Older CLI | This CLI |
|---|---|
| `--root <path>` | `--working-path <path>` |
| `--data-dir <dir>` | `--config-dir <dir>` (`--data-dir` still works) |
| `--force-update`, `--auto-update`, `canary` | `--force-update-check`, `--always-update`; no canary channel |
| `trash`, `trash restore`, `trash delete`, `trash empty` | `list-trash`, `trash-restore`, `trash-delete`, `empty-trash` |
| `links` | `list-public-links`, `public-link <path>` |
| `rm --no-trash` | `rm -p` |
| `statfs` | `statfs`, or `stat /` |
| `sync` (once), `sync --continuous` | `sync --once`, `sync` (continuous by default) |
| `webdav`, `s3` (0.0.0.0, port 80 or 443) | `serve webdav`, `serve s3` (127.0.0.1, port 1900 or 1800; `--addr 0.0.0.0:1900` for every interface) |
| `--w-user`, `--w-password`, `--s3-access-key-id`, `--s3-secret-access-key` | `--user`, `--password` |
| `--w-https`, `--s3-https` | `--self-signed`, or `--cert` and `--key` |

`webdav-proxy`, WebDAV digest authentication, `--w-threads`/`--s3-threads` and the `.filen-cli-credentials` file are gone.
`filen help migration` covers all of it, including why.
