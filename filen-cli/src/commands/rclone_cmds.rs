//! [cli-doc] managed-rclone
//! The Filen CLI includes a managed installation of Rclone, which can be used to [access Filen](https://rclone.org/filen).
//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.

use anyhow::{Context as _, Result};
use filen_rclone_wrapper::{
	rclone_installation::{RcloneInstallation, RcloneInstallationConfig},
	serve::BasicServerOptions,
};
use tokio::select;

use crate::{CliConfig, auth::LazyClient, ui::UI};

pub(crate) async fn mount(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	mount_point: Option<String>,
	cache_size: Option<String>,
	transfers: Option<usize>,
	rclone_args: Vec<String>,
) -> Result<()> {
	let client = client.get(ui).await?;
	let config_dir = config.config_dir.join("rclone");
	check_already_downloaded(ui, &config_dir).await;
	let mut network_drive = filen_rclone_wrapper::network_drive::NetworkDrive::mount(
		client,
		&RcloneInstallationConfig::new(&config_dir),
		mount_point.as_deref(),
		false,
		cache_size,
		transfers,
		rclone_args,
	)
	.await
	.context("Failed to mount network drive (use --verbose for more info)")?;
	RcloneInstallation::pipe_output_to_logs(&mut network_drive.process);
	network_drive
		.wait_until_active()
		.await
		.context("Failed to mount network drive (use --verbose for more info)")?;
	ui.print_success("Mounted network drive (kill the CLI to unmount and exit)");
	let mut stop_rx = crate::CTRLC_TX.subscribe();
	select! {
		_ = stop_rx.recv() => {
			ui.print_muted("Unmounting network drive...");
			network_drive.process.kill().await.context("Failed to kill mount process")?;
		}
		result = network_drive.process.wait() => {
			let status = result.context("Failed to wait for mount process")?;
			if !status.success() {
				return Err(anyhow::anyhow!(match status.code() {
					Some(c) => format!("Mount process exited with code: {}", c),
					None => "Mount process exited with unknown code".to_string(),
				}));
			}
		}
	}
	Ok(())
}

pub(crate) async fn start_server(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	server_type: &str,
	display_server_type: &str,
	options: BasicServerOptions,
	rclone_args: Vec<String>,
) -> Result<()> {
	let client = client.get(ui).await?;
	let config_dir = config.config_dir.join("rclone");
	check_already_downloaded(ui, &config_dir).await;
	let mut server = filen_rclone_wrapper::serve::start_basic_server(
		client,
		&RcloneInstallationConfig::new(&config_dir),
		server_type,
		options,
		rclone_args,
	)
	.await
	.with_context(|| format!("Failed to start {} server", display_server_type))?;
	RcloneInstallation::pipe_output_to_logs(&mut server.process);
	ui.print_success(&format!(
		"Started {} server on http://{} {} (kill the CLI to stop)",
		display_server_type,
		server.address,
		if let Some(auth) = &server.auth {
			format!(
				"with {} \"{}\" and {} \"{}\"",
				if server_type == "s3" {
					"Access Key ID"
				} else {
					"username"
				},
				auth.user,
				if server_type == "s3" {
					"Secret Access Key"
				} else {
					"password"
				},
				auth.password
			)
		} else {
			"without authentication".to_string()
		}
	));
	let mut stop_rx = crate::CTRLC_TX.subscribe();
	select! {
		_ = stop_rx.recv() => {
			ui.print_muted(&format!("Stopping {} server...", display_server_type));
			server.process.kill().await.with_context(|| {
				format!("Failed to kill {} server process", display_server_type)
			})?;
		}
		result = server.process.wait() => {
			let status = result.with_context(|| {
				format!("Failed to wait for {} server process", display_server_type)
			})?;
			if !status.success() {
				return Err(anyhow::anyhow!(match status.code() {
					Some(c) => format!(
						"{} server process exited with code: {} (use --verbose for more info)",
						display_server_type, c
					),
					None => format!(
						"{} server process exited with unknown code",
						display_server_type
					),
				}));
			}
		}
	}
	Ok(())
}

pub(crate) async fn execute_rclone(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	cmd: Vec<String>,
) -> Result<()> {
	let config_dir = config.config_dir.join("rclone");
	check_already_downloaded(ui, &config_dir).await;
	let rclone = filen_rclone_wrapper::rclone_installation::RcloneInstallation::initialize(
		&RcloneInstallationConfig::new(&config_dir),
		Some(client.get(ui).await?),
	)
	.await
	.context("Failed to initialize rclone installation")?;
	let exit_code = rclone
		.execute(&cmd.iter().map(|s| s.as_str()).collect::<Vec<&str>>())
		.await?
		.code();
	if let Some(exit_code) = exit_code
		&& exit_code != 0
	{
		return Err(crate::construct_exit_code_error(exit_code));
	}
	Ok(())
}

async fn check_already_downloaded(ui: &mut UI, config_dir: &std::path::Path) {
	if !filen_rclone_wrapper::rclone_installation::RcloneInstallation::check_already_downloaded(
		&RcloneInstallationConfig::new(config_dir),
	)
	.await
	{
		ui.print_muted("Downloading managed Rclone...");
	}
}
