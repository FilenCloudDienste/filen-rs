use anyhow::{Context, Result};
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

	async fn request<T: DeserializeOwned>(&self, endpoint: &str) -> Result<T> {
		let response = self
			.client
			.post(format!("{}/{}", self.root, endpoint))
			.send()
			.await
			.with_context(|| {
				format!(
					"Failed to send request to Rclone API at endpoint: {}",
					endpoint
				)
			})?;
		let response = response.bytes().await?;
		match serde_json::from_slice::<T>(&response) {
			Ok(result) => Ok(result),
			Err(_) => Err(anyhow::anyhow!(
				"Failed to parse Rclone response; response body: {}",
				String::from_utf8_lossy(&response)
			)),
		}
	}

	pub(crate) async fn core_stats(&self) -> Result<CoreStatsResponse> {
		self.request("core/stats").await
	}

	pub(crate) async fn vfs_stats(&self) -> Result<VfsStatsResponse> {
		self.request("vfs/stats").await
	}

	pub(crate) async fn vfs_list(&self) -> Result<VfsListResponse> {
		self.request("vfs/list").await
	}
}
