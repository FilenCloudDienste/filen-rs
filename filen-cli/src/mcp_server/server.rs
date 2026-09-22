// module written by Claude

use std::sync::Arc;

use crate::mcp_server::endpoints::{AnyEndpoint, all_endpoints};
use anyhow::Result;
use rmcp::{
	ErrorData, RoleServer, ServerHandler,
	handler::server::{
		router::tool::{ToolRoute, ToolRouter},
		tool::ToolCallContext,
	},
	model::{
		CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
		ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
	},
	service::RequestContext,
};

/// Convert a `schemars` schema into the shape `Tool` wants.
///
/// `#[tool]` routes schemas through `schema_for_input`/`schema_for_output`, which
/// strip the root `title`/`description` that `schemars` derives from the Rust type
/// name — those aren't part of the tool's contract and only confuse clients. Those
/// helpers are generic over a `JsonSchema` type, so they can't be used on a schema
/// that only exists as a value at runtime; this does the same stripping by hand.
fn schema_to_json_object(schema: schemars::Schema) -> Arc<JsonObject> {
	let serde_json::Value::Object(mut object) = schema.to_value() else {
		panic!("a schemars root schema is always a JSON object");
	};
	object.remove("title");
	object.remove("description");
	Arc::new(object)
}

/// The `tools/list` entry for one endpoint — what `#[tool(...)]` would have
/// generated at compile time, built from the registry instead.
fn tool_attr(endpoint: &dyn AnyEndpoint) -> Tool {
	let input_schema = schema_to_json_object(endpoint.input_schema());

	// MCP requires an object at the root of `inputSchema`. The macro path gets this
	// checked for it (and panics on e.g. a newtype over `i32`), so check it here too
	// rather than shipping a tool no client can call.
	assert_eq!(
		input_schema.get("type").and_then(serde_json::Value::as_str),
		Some("object"),
		"input schema of endpoint `{}` must have root type `object`",
		endpoint.name(),
	);

	Tool::new(endpoint.name(), endpoint.description(), input_schema)
		.with_raw_output_schema(schema_to_json_object(endpoint.output_schema()))
}

/// Build the route that dispatches a `tools/call` to this endpoint.
///
/// `ToolRoute::new_dyn` is the untyped constructor: it takes the tool description
/// plus a closure over the raw call context, so neither the parameter type nor the
/// return type has to be known statically.
fn endpoint_route(
	client: Arc<filen_sdk_rs::auth::Client>,
	endpoint: Arc<dyn AnyEndpoint>,
) -> ToolRoute<EndpointServer> {
	ToolRoute::new_dyn(
		tool_attr(endpoint.as_ref()),
		move |mut context: ToolCallContext<'_, EndpointServer>| {
			// `Parameters<T>` would take these out of the context and deserialize
			// them; here the endpoint does its own deserialization from JSON.
			let arguments = context.arguments.take().unwrap_or_default();
			let endpoint = endpoint.clone();
			let client = client.clone();

			Box::pin(async move {
				match endpoint
					.handle(&client, serde_json::Value::Object(arguments))
					.await
				{
					// Mirrors what `-> Json<T>` does in a macro tool: the value goes
					// into `structuredContent`, with a text copy in `content` for
					// clients that don't read structured output.
					Ok(output) => Ok(CallToolResult::structured(output).into()),
					// A tool-level error, so the message reaches the caller. Reserve
					// `Err(ErrorData)` for failures that aren't the tool's own.
					Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
						error.to_string(),
					)])
					.into()),
				}
			})
		},
	)
}

#[derive(Clone)]
struct EndpointServer {
	tool_router: ToolRouter<Self>,
}

impl EndpointServer {
	fn new(client: Arc<filen_sdk_rs::auth::Client>) -> Self {
		// What `#[tool_router]` generates, as a loop over the registry.
		let mut tool_router = ToolRouter::new();
		for endpoint in all_endpoints() {
			tool_router.add_route(endpoint_route(client.clone(), Arc::from(endpoint)));
		}
		Self { tool_router }
	}
}

/// What `#[tool_handler]` generates, written out.
impl ServerHandler for EndpointServer {
	fn get_info(&self) -> ServerConfig {
		ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
	}

	async fn list_tools(
		&self,
		_request: Option<PaginatedRequestParams>,
		_context: RequestContext<RoleServer>,
	) -> Result<ListToolsResult, ErrorData> {
		Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
	}

	fn get_tool(&self, name: &str) -> Option<Tool> {
		self.tool_router.get(name).cloned()
	}

	async fn call_tool(
		&self,
		request: CallToolRequestParams,
		context: RequestContext<RoleServer>,
	) -> Result<CallToolResponse, ErrorData> {
		self.tool_router
			.call(ToolCallContext::new(self, request, context))
			.await
	}
}

pub(crate) async fn run_mcp_server(client: Arc<filen_sdk_rs::auth::Client>) -> Result<()> {
	let service = rmcp::serve_server(EndpointServer::new(client), rmcp::transport::stdio()).await?;
	service.waiting().await?;
	Ok(())
}
