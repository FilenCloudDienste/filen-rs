//! A client for an rclone daemon's remote control API (rc) on loopback.
//!
//! Every call is a JSON `POST` with basic auth. Neither a request nor a response body is ever
//! logged, and a failed call carries only rc's `error` line: `config/create` bodies hold the
//! account's keys, and rc echoes a failed call's input back in its answer.

use std::{fmt, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use crate::{Error, ErrorKind};

/// How long one rc call may take before it fails. Mounting is the slowest call rc answers, and
/// it is bounded well below this; the limit only keeps a wedged rclone from hanging a caller.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest rc `error` line a failed call keeps.
const MAX_ERROR_LEN: usize = 1024;

/// An rc call rclone answered with an error status.
#[derive(Debug, thiserror::Error)]
#[error("rclone rc {path} failed with status {status}: {message}")]
pub(crate) struct RcCallFailed {
	/// The rc method, such as `mount/mount`.
	pub(crate) path: &'static str,
	/// The HTTP status rclone answered with: 401 for missing or wrong credentials, 404 for an
	/// unknown method, 500 for a method that failed.
	pub(crate) status: u16,
	/// rc's `error` line, cut at [`MAX_ERROR_LEN`] bytes, or a note that the answer had none.
	pub(crate) message: String,
}

pub(crate) struct RcClient {
	http: reqwest::Client,
	/// rc's port on 127.0.0.1.
	port: u16,
	user: String,
	pass: String,
}

impl fmt::Debug for RcClient {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RcClient")
			.field("port", &self.port)
			.field("user", &self.user)
			.finish_non_exhaustive()
	}
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreVersion {
	/// Such as `v1.74.3`.
	pub(crate) version: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreStats {
	/// The transfers running now; absent when there are none.
	#[serde(default)]
	pub(crate) transferring: Vec<CoreStatsTransfer>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreStatsTransfer {
	pub(crate) name: String,
	/// Bytes transferred so far.
	pub(crate) bytes: i64,
	/// The file's size in bytes, negative when rclone does not know it.
	pub(crate) size: i64,
	/// Bytes per second.
	pub(crate) speed: f64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VfsList {
	pub(crate) vfses: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VfsStats {
	/// Absent for a VFS without a disk cache.
	pub(crate) disk_cache: Option<VfsDiskCache>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VfsDiskCache {
	pub(crate) bytes_used: i64,
	pub(crate) uploads_in_progress: i64,
	pub(crate) uploads_queued: i64,
	pub(crate) errored_files: i64,
	pub(crate) out_of_space: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListMounts {
	pub(crate) mount_points: Vec<ListedMount>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListedMount {
	#[serde(rename = "MountPoint")]
	pub(crate) mount_point: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServeStarted {
	pub(crate) id: String,
	/// The address it listens on.
	pub(crate) addr: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServeList {
	pub(crate) list: Vec<ListedServe>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListedServe {
	pub(crate) id: String,
	pub(crate) addr: String,
	/// The parameters it was started with, of which only the type is read: the others include
	/// its credentials.
	pub(crate) params: ListedServeParams,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListedServeParams {
	#[serde(rename = "type")]
	pub(crate) kind: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServeTypes {
	pub(crate) types: Vec<String>,
}

/// What a call answered with when the caller needs nothing from it.
#[derive(Debug, Deserialize)]
pub(crate) struct Ignored {}

impl RcClient {
	pub(crate) fn new(port: u16, user: &str, pass: &str) -> Result<Self, Error> {
		let http = reqwest::Client::builder()
			// rc is on loopback: a proxy from the environment must never see its credentials.
			.no_proxy()
			.timeout(CALL_TIMEOUT)
			.build()
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					e,
					Some("failed to build the rclone rc client"),
				)
			})?;
		Ok(Self {
			http,
			port,
			user: user.to_owned(),
			pass: pass.to_owned(),
		})
	}

	#[cfg(feature = "test-seams")]
	pub(crate) fn port(&self) -> u16 {
		self.port
	}

	/// Calls the rc method `path` with the JSON object `body`.
	pub(crate) async fn call<T: DeserializeOwned>(
		&self,
		path: &'static str,
		body: &Value,
	) -> Result<T, Error> {
		let response = self
			.http
			.post(format!("http://127.0.0.1:{}/{path}", self.port))
			.basic_auth(&self.user, Some(&self.pass))
			.json(body)
			.send()
			.await
			.map_err(|e| Error::from(e).with_context(format!("rclone rc {path}")))?;
		let status = response.status();
		let bytes = response
			.bytes()
			.await
			.map_err(|e| Error::from(e).with_context(format!("rclone rc {path}")))?;
		if !status.is_success() {
			return Err(Error::custom_with_source(
				ErrorKind::IO,
				RcCallFailed {
					path,
					status: status.as_u16(),
					message: error_line(&bytes),
				},
				None::<&str>,
			));
		}
		serde_json::from_slice(&bytes)
			.map_err(|e| Error::from(e).with_context(format!("rclone rc {path}'s answer")))
	}

	pub(crate) async fn core_version(&self) -> Result<CoreVersion, Error> {
		self.call("core/version", &json!({})).await
	}

	pub(crate) async fn core_stats(&self) -> Result<CoreStats, Error> {
		self.call("core/stats", &json!({})).await
	}

	pub(crate) async fn core_quit(&self) -> Result<(), Error> {
		self.call::<Ignored>("core/quit", &json!({})).await?;
		Ok(())
	}

	pub(crate) async fn vfs_list(&self) -> Result<VfsList, Error> {
		self.call("vfs/list", &json!({})).await
	}

	pub(crate) async fn vfs_stats(&self, fs: &str) -> Result<VfsStats, Error> {
		self.call("vfs/stats", &json!({ "fs": fs })).await
	}

	/// Creates the remote `name` of backend `kind` in rclone's config, which the daemon keeps in
	/// memory. Values of the backend's password options are given in plain text: rclone obscures
	/// them itself (`obscure`), and never asks anything back (`nonInteractive`).
	pub(crate) async fn config_create(
		&self,
		name: &str,
		kind: &str,
		parameters: &Map<String, Value>,
	) -> Result<(), Error> {
		self.call::<Ignored>(
			"config/create",
			&json!({
				"name": name,
				"type": kind,
				"parameters": parameters,
				"opt": { "obscure": true, "nonInteractive": true },
			}),
		)
		.await?;
		Ok(())
	}

	/// Mounts `fs` at `mount_point` with the `mountType` `mount_type` and the given options.
	pub(crate) async fn mount_mount(
		&self,
		fs: &str,
		mount_point: &str,
		mount_type: &str,
		vfs_opt: &Value,
		mount_opt: &Value,
	) -> Result<(), Error> {
		self.call::<Ignored>(
			"mount/mount",
			&json!({
				"fs": fs,
				"mountPoint": mount_point,
				"mountType": mount_type,
				"vfsOpt": vfs_opt,
				"mountOpt": mount_opt,
			}),
		)
		.await?;
		Ok(())
	}

	pub(crate) async fn mount_unmount(&self, mount_point: &str) -> Result<(), Error> {
		self.call::<Ignored>("mount/unmount", &json!({ "mountPoint": mount_point }))
			.await?;
		Ok(())
	}

	pub(crate) async fn mount_listmounts(&self) -> Result<ListMounts, Error> {
		self.call("mount/listmounts", &json!({})).await
	}

	pub(crate) async fn mount_unmountall(&self) -> Result<(), Error> {
		self.call::<Ignored>("mount/unmountall", &json!({})).await?;
		Ok(())
	}

	/// Starts a serve with the JSON object `params`: its `type`, `fs` and `addr`, and its
	/// command-line flags as `serve/start` takes them (see `vfs::vfs_flags`).
	pub(crate) async fn serve_start(&self, params: &Value) -> Result<ServeStarted, Error> {
		self.call("serve/start", params).await
	}

	pub(crate) async fn serve_stop(&self, id: &str) -> Result<(), Error> {
		self.call::<Ignored>("serve/stop", &json!({ "id": id }))
			.await?;
		Ok(())
	}

	pub(crate) async fn serve_list(&self) -> Result<ServeList, Error> {
		self.call("serve/list", &json!({})).await
	}

	pub(crate) async fn serve_types(&self) -> Result<ServeTypes, Error> {
		self.call("serve/types", &json!({})).await
	}

	pub(crate) async fn serve_stopall(&self) -> Result<(), Error> {
		self.call::<Ignored>("serve/stopall", &json!({})).await?;
		Ok(())
	}
}

/// rc's `error` line from a failed call's answer, never the rest of it: the answer echoes the
/// call's input.
fn error_line(body: &[u8]) -> String {
	#[derive(Deserialize)]
	struct Failed {
		error: String,
	}
	match serde_json::from_slice::<Failed>(body) {
		Ok(Failed { mut error }) => {
			if error.len() > MAX_ERROR_LEN {
				let end = error.floor_char_boundary(MAX_ERROR_LEN);
				error.truncate(end);
			}
			error
		}
		Err(_) => "(no error line in rclone's answer)".to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_failed_answer_keeps_only_its_error_line() {
		let body = br#"{"error":"mount failed","input":{"pass":"hunter2"},"path":"mount/mount","status":500}"#;

		assert_eq!(error_line(body), "mount failed");
	}

	#[test]
	fn an_answer_without_an_error_line_says_so() {
		assert_eq!(
			error_line(b"<html>pass=hunter2</html>"),
			"(no error line in rclone's answer)"
		);
	}

	#[test]
	fn a_long_error_line_is_cut_on_a_char_boundary() {
		let body = format!(r#"{{"error":"{}"}}"#, "é".repeat(MAX_ERROR_LEN));

		let line = error_line(body.as_bytes());

		assert_eq!(line.len(), MAX_ERROR_LEN);
		assert_eq!(line, "é".repeat(MAX_ERROR_LEN / 2));
	}

	#[test]
	fn debug_never_prints_the_password() {
		let rc = RcClient::new(5572, "filen", "hunter2").unwrap();

		let printed = format!("{rc:?}");

		assert!(!printed.contains("hunter2"), "{printed}");
		assert_eq!(printed, r#"RcClient { port: 5572, user: "filen", .. }"#);
	}
}
