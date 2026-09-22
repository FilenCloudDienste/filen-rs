use anyhow::Result;

mod add;
mod subtract;

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
	fn handle(&self, input: Self::Input) -> Result<Self::Output>;
}

pub trait AnyEndpoint: Send + Sync {
	fn name(&self) -> &'static str;
	fn description(&self) -> &'static str;
	fn input_schema(&self) -> schemars::Schema;
	fn output_schema(&self) -> schemars::Schema;
	fn handle(&self, input: serde_json::Value) -> Result<serde_json::Value>;
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

	fn handle(&self, input: serde_json::Value) -> Result<serde_json::Value> {
		let input = serde_json::from_value(input)?;
		let output = <T as Endpoint>::handle(self, input)?;
		Ok(serde_json::to_value(output)?)
	}
}

pub fn all_endpoints() -> Vec<Box<dyn AnyEndpoint>> {
	vec![Box::new(add::Add), Box::new(subtract::Subtract)]
}
