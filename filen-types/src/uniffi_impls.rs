type DateTime = chrono::DateTime<chrono::Utc>;
#[cfg(feature = "uniffi")]
uniffi::custom_type!(DateTime, i64, {
	remote,
	lower: |dt: &DateTime| dt.timestamp_millis(),
	try_lift: |millis: i64| {
		chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis).ok_or_else(|| uniffi::deps::anyhow::anyhow!("invalid timestamp millis: {}", millis))
	},
});
