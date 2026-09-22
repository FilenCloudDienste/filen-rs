use crate::mcp_server::endpoints::Endpoint;
use anyhow::Result;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AddInput {
	a: i32,
	b: i32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AddOutput {
	result: i32,
}

pub(crate) struct Add;

impl Endpoint for Add {
	type Input = AddInput;
	type Output = AddOutput;

	fn description() -> &'static str {
		"Adds two numbers"
	}

	fn handle(&self, input: Self::Input) -> Result<Self::Output> {
		Ok(AddOutput {
			result: input.a + input.b,
		})
	}
}
