use anyhow::{Result, anyhow};
use filen_sdk_rs::auth::Client;
use std::path::Path;
use tokio::process::Child;

use crate::rclone_installation::RcloneInstallation;

pub struct BasicAuthentication {
	pub user: String,
	pub password: String,
}

pub struct BasicServerOptions {
	pub url: Option<String>,
	pub root: Option<String>,
	pub user: Option<String>,
	pub password: Option<String>,
	pub read_only: bool,
}

pub struct BasicServerDetails {
	pub url: String,
	pub auth: Option<BasicAuthentication>,
	/// Will be killed on drop.
	pub process: Child,
}

pub async fn start_basic_server(
	client: &Client,
	config_dir: &Path,
	server_type: &str,
	options: BasicServerOptions,
) -> Result<BasicServerDetails> {
	if !["webdav", "ftp", "sftp", "http"].contains(&server_type) {
		return Err(anyhow!("Unsupported server type: {}", server_type));
	}
	let url = options.url.unwrap_or("127.0.0.1:8080".to_string());
	let auth: Option<BasicAuthentication> = if options.user.is_some() || options.password.is_some()
	{
		Some(BasicAuthentication {
			user: options.user.unwrap_or("user".to_string()),
			password: options.password.unwrap_or("password".to_string()),
		})
	} else {
		None
	};
	let (process, _) = RcloneInstallation::initialize(client, config_dir)
		.await?
		.execute_in_background(&[
			"serve",
			server_type,
			&format!("filen:{}", options.root.unwrap_or("".to_string())),
			"--addr",
			&url,
			if auth.is_some() { "--user" } else { "" },
			if let Some(auth) = &auth {
				auth.user.as_str()
			} else {
				""
			},
			if auth.is_some() { "--pass" } else { "" },
			if let Some(auth) = &auth {
				auth.password.as_str()
			} else {
				""
			},
			if options.read_only { "--read-only" } else { "" },
		])
		.await?;
	Ok(BasicServerDetails { url, auth, process })
}
