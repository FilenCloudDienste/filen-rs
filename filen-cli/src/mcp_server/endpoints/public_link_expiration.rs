/// Mirrors `filen_types::api::v3::dir::link::PublicLinkExpiration` for MCP schemas, since the
/// upstream type doesn't derive `schemars::JsonSchema`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) enum PublicLinkExpiration {
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

impl From<PublicLinkExpiration> for filen_types::api::v3::dir::link::PublicLinkExpiration {
	fn from(value: PublicLinkExpiration) -> Self {
		match value {
			PublicLinkExpiration::Never => Self::Never,
			PublicLinkExpiration::OneHour => Self::OneHour,
			PublicLinkExpiration::SixHours => Self::SixHours,
			PublicLinkExpiration::OneDay => Self::OneDay,
			PublicLinkExpiration::ThreeDays => Self::ThreeDays,
			PublicLinkExpiration::OneWeek => Self::OneWeek,
			PublicLinkExpiration::TwoWeeks => Self::TwoWeeks,
			PublicLinkExpiration::ThirtyDays => Self::ThirtyDays,
		}
	}
}

impl From<filen_types::api::v3::dir::link::PublicLinkExpiration> for PublicLinkExpiration {
	fn from(value: filen_types::api::v3::dir::link::PublicLinkExpiration) -> Self {
		use filen_types::api::v3::dir::link::PublicLinkExpiration as Upstream;
		match value {
			Upstream::Never => Self::Never,
			Upstream::OneHour => Self::OneHour,
			Upstream::SixHours => Self::SixHours,
			Upstream::OneDay => Self::OneDay,
			Upstream::ThreeDays => Self::ThreeDays,
			Upstream::OneWeek => Self::OneWeek,
			Upstream::TwoWeeks => Self::TwoWeeks,
			Upstream::ThirtyDays => Self::ThirtyDays,
		}
	}
}
