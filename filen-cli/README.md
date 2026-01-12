# Filen CLI (public beta)

📩 [Releases](https://github.com/FilenCloudDienste/filen-cli-releases/releases) | 📖 [Documentation](https://docs.filen.io/docs/cli-rs/readme) | 📜 [Source](https://github.com/FilenCloudDienste/filen-rs/tree/main/filen-cli)

The Filen CLI provides a set of useful tools for interacting with your Filen cloud drive, like managing files and directories.
Start it without specifying a command to enter interactive mode.
For now, syncing, drive mounting, etc. are available through the managed [Filen Rclone](https://github.com/FilenCloudDienste/filen-rclone).

> [!Important]
> **Public Beta:**
> This is the Rust rewrite of [`FilenCloudDienste/filen-cli`](https://github.com/FilenCloudDienste/filen-cli), which has been [sunsetted](https://github.com/FilenCloudDienste/filen-cli?tab=readme-ov-file#sunsetting-filen-cli).
> While it aims to fully replace it, it is currently in open beta: Some functionality is still missing, and *there might be bugs*. 

> [!Note]
> Please **report bugs** on our [issues page at `filen-rs`](https://github.com/FilenCloudDienste/filen-rs/issues) by [including "[CLI]" in the title](https://github.com/FilenCloudDienste/filen-rs/issues/new?title=[CLI]%20)! \
> **Feature requests** can be submitted on [features.filen.io](https://features.filen.io/?tags=cli).

## Installation and updates

You can download the latest binaries from the [release page](https://github.com/FilenCloudDienste/filen-cli-releases/releases/latest).  
Docker images are also available as [`filen/cli`](https://hub.docker.com/repository/docker/filen/cli) (you need to specify a version instead of using `:latest`).

The CLI includes an automatic updater.

## Documentation

You can find documentation from within the CLI using the `help` subcommand and at [docs.filen.io](https://docs.filen.io/docs/cli-rs/readme/).