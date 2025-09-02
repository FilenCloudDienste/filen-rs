use serde::{Deserialize, Serialize};

pub mod add;
pub mod content;
pub mod edit;
pub mod info;
pub mod remove;
pub mod status;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	derive(tsify::Tsify)
)]
pub enum PublicLinkExpiration {
	#[serde(rename = "never")]
	Never,
	#[serde(rename = "1h")]
	OneHour,
	#[serde(rename = "6h")]
	SixHours,
	#[serde(rename = "1d")]
	OneDay,
	#[serde(rename = "3d")]
	ThreeDays,
	#[serde(rename = "7d")]
	OneWeek,
	#[serde(rename = "14d")]
	TwoWeeks,
	#[serde(rename = "30d")]
	ThirtyDays,
}
