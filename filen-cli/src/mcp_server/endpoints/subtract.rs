use crate::mcp_server::endpoints::Endpoint;
use anyhow::Result;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct SubtractInput {
	a: i32,
	b: i32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct SubtractOutput {
	result: i32,
}

pub(crate) struct Subtract;

impl Endpoint for Subtract {
	type Input = SubtractInput;
	type Output = SubtractOutput;

	fn description() -> &'static str {
		"Subtracts two numbers"
	}

	fn handle(&self, input: Self::Input) -> Result<Self::Output> {
		Ok(SubtractOutput {
			result: input.a - input.b,
		})
	}
}
