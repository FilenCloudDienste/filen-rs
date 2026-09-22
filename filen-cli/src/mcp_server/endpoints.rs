use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

mod read_directory;

pub trait Endpoint {
	type Input: serde::Serialize + serde::de::DeserializeOwned + schemars::JsonSchema;
	type Output: serde::Serialize + serde::de::DeserializeOwned + schemars::JsonSchema;

	fn name() -> &'static str {
		// defaults to the implementing type's name
		let path = std::any::type_name::<Self>();
		path.rsplit("::").next().unwrap_or(path)
	}
	fn description() -> &'static str {
		"No description provided"
	}
	fn input_schema() -> schemars::Schema {
		schemars::schema_for!(Self::Input)
	}
	fn output_schema() -> schemars::Schema {
		schemars::schema_for!(Self::Output)
	}
	fn handle(
		&self,
		client: &filen_sdk_rs::auth::Client,
		input: Self::Input,
	) -> impl Future<Output = Result<Self::Output>> + Send;
}

pub trait AnyEndpoint: Send + Sync {
	fn name(&self) -> &'static str;
	fn description(&self) -> &'static str;
	fn input_schema(&self) -> schemars::Schema;
	fn output_schema(&self) -> schemars::Schema;
	fn handle<'a>(
		&'a self,
		client: &'a filen_sdk_rs::auth::Client,
		input: serde_json::Value,
	) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>>;
}

impl<T: Endpoint + Send + Sync> AnyEndpoint for T {
	fn name(&self) -> &'static str {
		<T as Endpoint>::name()
	}

	fn description(&self) -> &'static str {
		<T as Endpoint>::description()
	}

	fn input_schema(&self) -> schemars::Schema {
		<T as Endpoint>::input_schema()
	}

	fn output_schema(&self) -> schemars::Schema {
		<T as Endpoint>::output_schema()
	}

	fn handle<'a>(
		&'a self,
		client: &'a filen_sdk_rs::auth::Client,
		input: serde_json::Value,
	) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>> {
		Box::pin(async move {
			let input = serde_json::from_value(input)?;
			let output = <T as Endpoint>::handle(self, client, input).await?;
			Ok(serde_json::to_value(output)?)
		})
	}
}

pub fn all_endpoints() -> Vec<Box<dyn AnyEndpoint>> {
	vec![Box::new(read_directory::ReadDirectory)]
}
