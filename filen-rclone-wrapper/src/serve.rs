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
	pub address: String,
	pub root: Option<String>,
	pub user: Option<String>,
	pub password: Option<String>,
	pub read_only: bool,
	pub cache_size: Option<String>,
	pub transfers: Option<usize>,
}

pub struct BasicServerDetails {
	pub address: String,
	pub auth: Option<BasicAuthentication>,
	/// Will be killed on drop.
	pub process: Child,
}

/// Starts a "webdav", "ftp", "sftp", "http" or "s3" server over Rclone.
/// "s3" server options take the Access Key ID as `options.user` and Secret Access Key as `options.password`.
pub async fn start_basic_server(
	client: &Client,
	config_dir: &Path,
	server_type: &str,
	options: BasicServerOptions,
) -> Result<BasicServerDetails> {
	if !["webdav", "ftp", "sftp", "http", "s3"].contains(&server_type) {
		return Err(anyhow!("Unsupported server type: {}", server_type));
	}
	let address = if options.address.starts_with(":") {
		format!("127.0.0.1{}", options.address)
	} else {
		options.address.clone()
	};
	let remote_str = format!("filen:{}", options.root.unwrap_or("".to_string()));
	let mut args = vec![
		"serve",
		server_type,
		&remote_str,
		"--addr",
		&address,
		if options.read_only { "--read-only" } else { "" },
	];
	let cache_args =
		RcloneInstallation::construct_cache_args(config_dir, options.cache_size.clone())?;
	args.extend(cache_args.split(' '));
	let transfers_str;
	if let Some(t) = options.transfers.map(|t| t.to_string()) {
		transfers_str = t;
		args.push("--transfers");
		args.push(&transfers_str);
	}
	let (user, password) = (
		options.user.clone().unwrap_or("user".to_string()),
		options.password.clone().unwrap_or("password".to_string()),
	);
	let auth_key_str = format!("{},{}", user, password);
	if server_type == "s3" {
		args.extend(["--auth-key", &auth_key_str]);
	} else if options.user.is_some() || options.password.is_some() {
		args.extend(["--user", &user, "--pass", &password]);
	}
	let (process, _) = RcloneInstallation::initialize(client, config_dir)
		.await?
		.execute_in_background(&args)
		.await?;
	Ok(BasicServerDetails {
		address,
		auth: if server_type == "s3" || (options.user.is_some() || options.password.is_some()) {
			Some(BasicAuthentication {
				user: user.clone(),
				password: password.clone(),
			})
		} else {
			None
		},
		process,
	})
}
