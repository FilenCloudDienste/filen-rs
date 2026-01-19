use anyhow::{Context, Result};
use log::debug;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CoreStatsResponse {
	pub(crate) transferring: Option<Vec<CoreStatsResponseTransfer>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CoreStatsResponseTransfer {
	pub(crate) name: String,
	/// in bytes
	pub(crate) size: i64,
	/// in bytes per second
	pub(crate) speed: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VfsStatsResponse {
	pub(crate) disk_cache: VfsStatsResponseDiskCache,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VfsStatsResponseDiskCache {
	pub(crate) uploads_in_progress: i32,
	pub(crate) uploads_queued: i32,
	pub(crate) errored_files: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct VfsListResponse {
	pub(crate) vfses: Vec<String>,
}

pub(crate) struct RcloneApiClient {
	client: reqwest::Client,
	root: String,
}

impl RcloneApiClient {
	pub(crate) fn new(port: u16) -> Self {
		Self {
			client: reqwest::Client::new(),
			root: format!("http://127.0.0.1:{}", port),
		}
	}

	async fn request<T: DeserializeOwned>(
		&self,
		endpoint: &str,
		body: Option<serde_json::Value>,
	) -> Result<T> {
		let response = self
			.client
			.post(format!("{}/{}", self.root, endpoint))
			.body(if let Some(body) = body {
				body.to_string()
			} else {
				"".to_string()
			})
			.send()
			.await
			.with_context(|| {
				format!(
					"Failed to send request to Rclone API at endpoint: {}",
					endpoint
				)
			})?;
		let response = response.bytes().await?;
		debug!(
			"endpoint {} response: {}",
			endpoint,
			String::from_utf8_lossy(&response)
		);
		match serde_json::from_slice::<T>(&response) {
			Ok(result) => Ok(result),
			Err(_) => Err(anyhow::anyhow!(
				"Failed to parse Rclone response; response body: {}",
				String::from_utf8_lossy(&response)
			)),
		}
	}

	pub(crate) async fn core_stats(&self) -> Result<CoreStatsResponse> {
		self.request("core/stats", None).await
	}

	pub(crate) async fn vfs_stats(&self) -> Result<VfsStatsResponse> {
		self.request("vfs/stats", None).await
	}

	pub(crate) async fn vfs_list(&self) -> Result<VfsListResponse> {
		self.request("vfs/list", None).await
	}
}

// Returns when the serve/list endpoint returns at least one server.
/* pub(crate) async fn wait_until_active_server(
	api: &RcloneApiClient,
	timeout: Duration,
) -> Result<()> {
	let mut elapsed = 0;
	let interval = 100; // ms
	loop {
		match api.serve_list().await {
			Ok(response) => {
				dbg!(response.clone());
				if !response.list.is_empty() {
					break;
				} else {
					trace!("Rclone serve/list endpoint returned no active servers yet");
				}
			}
			Err(e) => {
				warn!("Failed to query rclone serve/list endpoint: {}", e);
			}
		}
		if elapsed >= timeout.as_millis() as u64 {
			return Err(anyhow::anyhow!(
				"Timed out waiting for Rclone server to become active"
			));
		}
		tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
		elapsed += interval;
	}
	Ok(())
} */
