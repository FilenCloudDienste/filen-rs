use std::num::NonZeroU32;

mod download;
mod upload;

const BANDWIDTH_CHUNK_SIZE_KB: NonZeroU32 = NonZeroU32::new(16).unwrap();
const BANDWIDTH_CHUNK_USIZE_KB: usize = BANDWIDTH_CHUNK_SIZE_KB.get() as usize;

const BYTES_PER_KILOBYTE: NonZeroU32 = NonZeroU32::new(1024).unwrap();
const BYTES_PER_KILOBYTE_USIZE: usize = BYTES_PER_KILOBYTE.get() as usize;

pub(crate) use download::{DownloadBandwidthLimiterLayer, new_download_bandwidth_limiter};
pub(crate) use upload::{UploadBandwidthLimiterLayer, new_upload_bandwidth_limiter};
