# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

- rclone commands (`mount`, `serve`) are more customizable via `--cache-size` and `--transfers` flags
  as well as the option to take any number of custom Rclone options
- convenient install script on Linux and macOS

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
