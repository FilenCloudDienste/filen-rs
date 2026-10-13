use crate::{
	rpc_servers::endpoints::{AnyEndpoint, all_endpoints},
	ui::UI,
};
use jsonrpsee::{
	server::{RpcModule, Server},
	types::{ErrorObjectOwned, Params, error::INVALID_PARAMS_CODE},
};
use openrpc_types::{
	Components, ContentDescriptor, Info, Method, OpenRPC, ParamStructure, ReferenceOr,
};
use schemars::{Schema, SchemaGenerator, generate::SchemaSettings, transform::RecursiveTransform};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};
use tokio::select;

/// Error code for failures reported by an endpoint itself (as opposed to malformed requests).
///
/// JSON-RPC reserves -32768..=-32000 for protocol and implementation-defined server errors
/// (jsonrpsee uses that range itself), so application errors get a code outside of it.
const ENDPOINT_ERROR_CODE: i32 = 1;
const ENDPOINT_ERROR_NAME: &str = "EndpointError";

/// Where the OpenRPC document keeps shared schemas; the generator is pointed here so that the
/// `$ref`s it emits resolve within the document.
const SCHEMAS_POINTER: &str = "/components/schemas";

/// Removes the root keys `root_schema_for` adds that don't belong in an embedded schema: the
/// meta-schema, the Rust type name as `title`, and the copy of all definitions generated so far
/// (those are collected once, into `components.schemas`).
fn strip_root(schema: &mut Schema) {
	let object = schema
		.as_object_mut()
		.expect("a schemars root schema is always a JSON object");
	object.remove("$schema");
	object.remove("title");
	object.remove("components");
}

/// Splits an endpoint's input object into one by-name param per property.
fn params_from_input_schema(
	endpoint: &dyn AnyEndpoint,
	mut input: Schema,
) -> Vec<ContentDescriptor> {
	let object = input
		.as_object_mut()
		.expect("a schemars root schema is always a JSON object");

	// Same requirement as for MCP tools: the input has to be an object, since each of its
	// properties becomes a named param.
	assert_eq!(
		object.get("type").and_then(serde_json::Value::as_str),
		Some("object"),
		"input schema of endpoint `{}` must have root type `object`",
		endpoint.name(),
	);

	let required: Vec<String> = match object.remove("required") {
		Some(required) => serde_json::from_value(required)
			.expect("`required` of a schemars object schema is a list of strings"),
		None => Vec::new(),
	};
	let properties = match object.remove("properties") {
		Some(serde_json::Value::Object(properties)) => properties,
		Some(_) => panic!("`properties` of a schemars object schema is always an object"),
		None => Default::default(),
	};

	let mut params: Vec<ContentDescriptor> = properties
		.into_iter()
		.map(|(name, schema)| {
			let mut schema =
				Schema::try_from(schema).expect("property schemas are objects or booleans");
			// The description belongs to the param rather than its type.
			let description = schema
				.as_object_mut()
				.and_then(|object| object.remove("description"))
				.and_then(|description| description.as_str().map(str::to_string));
			ContentDescriptor {
				required: Some(required.contains(&name)),
				name,
				description,
				schema,
				..Default::default()
			}
		})
		.collect();
	// The spec requires optional params to come after all required ones.
	params.sort_by_key(|param| param.required != Some(true));
	params
}

fn method_for(endpoint: &dyn AnyEndpoint, generator: &mut SchemaGenerator) -> Method {
	let mut input = endpoint.input_schema_with(generator);
	strip_root(&mut input);

	let mut output = endpoint.output_schema_with(generator);
	strip_root(&mut output);
	// Derived from the Rust type's doc comment; the method's own description covers it.
	output.remove("description");

	Method {
		name: endpoint.name().to_string(),
		description: Some(endpoint.description().to_string()),
		params: params_from_input_schema(endpoint, input)
			.into_iter()
			.map(ReferenceOr::Item)
			.collect(),
		result: Some(ReferenceOr::Item(ContentDescriptor {
			name: "result".to_string(),
			schema: output,
			..Default::default()
		})),
		errors: Some(vec![ReferenceOr::Reference(format!(
			"#/components/errors/{ENDPOINT_ERROR_NAME}"
		))]),
		param_structure: Some(ParamStructure::ByName),
		..Default::default()
	}
}

/// Keywords that annotate a schema as a whole rather than constrain one of its types; they stay
/// on the outer schema when [`nullable_type_to_any_of`] splits it.
const ANNOTATION_KEYWORDS: &[&str] = &[
	"title",
	"description",
	"default",
	"examples",
	"deprecated",
	"readOnly",
	"writeOnly",
	"$comment",
];

/// Rewrites a nullable type list like `{"type": ["integer", "null"], "minimum": 0}` into the
/// equivalent `{"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}]}`.
///
/// Both are valid JSON Schema, and schemars emits the former for every `Option<T>` of a
/// primitive `T`. But tooling built on the OpenRPC spec handles `type` lists poorly (e.g.
/// `openrpc-md` silently drops such fields), while `anyOf` is supported everywhere.
fn nullable_type_to_any_of(schema: &mut Schema) {
	let Some(object) = schema.as_object_mut() else {
		return;
	};
	let Some(serde_json::Value::Array(types)) = object.get("type") else {
		return;
	};
	if !types.iter().any(|ty| ty == "null") {
		return;
	}
	let mut non_null: Vec<serde_json::Value> =
		types.iter().filter(|ty| *ty != "null").cloned().collect();
	let non_null_type = match non_null.len() {
		0 => return,
		1 => non_null.remove(0),
		_ => serde_json::Value::Array(non_null),
	};

	// Everything but the annotations constrains the non-null type (`format`, `minimum`,
	// `items`, `properties`, ...), so it moves into that branch.
	let mut branch = std::mem::take(object);
	for keyword in ANNOTATION_KEYWORDS {
		if let Some(value) = branch.remove(*keyword) {
			object.insert(keyword.to_string(), value);
		}
	}
	branch.insert("type".to_string(), non_null_type);
	object.insert(
		"anyOf".to_string(),
		serde_json::json!([branch, { "type": "null" }]),
	);
}

/// Describes the endpoints as an OpenRPC document (https://spec.open-rpc.org), for serving from
/// `rpc.discover`.
fn build_openrpc_document(endpoints: &[Arc<dyn AnyEndpoint>], addr: SocketAddr) -> OpenRPC {
	// OpenRPC schemas are JSON Schema draft 7. All endpoints share one generator, so a type used
	// by several of them is defined once, and schemars disambiguates distinct types that happen
	// to share a name.
	let mut settings = SchemaSettings::draft07();
	settings.definitions_path = SCHEMAS_POINTER.into();
	settings.meta_schema = None;
	let mut generator = settings
		.with_transform(RecursiveTransform(nullable_type_to_any_of))
		.into_generator();

	let methods = endpoints
		.iter()
		.map(|endpoint| ReferenceOr::Item(method_for(endpoint.as_ref(), &mut generator)))
		.collect();

	let schemas = generator
		.take_definitions(true)
		.into_iter()
		.map(|(name, schema)| {
			let schema = Schema::try_from(schema).expect("definitions are objects or booleans");
			(name, schema)
		})
		.collect();

	OpenRPC {
		info: Info {
			title: "Filen CLI".to_string(),
			description: Some("Access to a Filen drive, served by the Filen CLI".to_string()),
			version: env!("CARGO_PKG_VERSION").to_string(),
			..Default::default()
		},
		servers: Some(vec![openrpc_types::Server {
			name: "filen-cli".to_string(),
			url: format!("http://{addr}"),
			..Default::default()
		}]),
		methods,
		components: Some(Components {
			schemas: Some(schemas),
			errors: Some(BTreeMap::from([(
				ENDPOINT_ERROR_NAME.to_string(),
				openrpc_types::Error {
					code: ENDPOINT_ERROR_CODE.into(),
					message: "The operation failed".to_string(),
					data: None,
				},
			)])),
			..Default::default()
		}),
		..Default::default()
	}
}

fn invalid_params(message: impl Into<String>) -> ErrorObjectOwned {
	ErrorObjectOwned::owned(INVALID_PARAMS_CODE, message, None::<()>)
}

/// Turns JSON-RPC params into an endpoint's input object. Only by-name params are supported,
/// since each endpoint takes a single object; omitted params count as an empty object.
fn input_from_params(params: &Params<'_>) -> Result<serde_json::Value, ErrorObjectOwned> {
	if params.as_str().is_none() {
		return Ok(serde_json::Value::Object(Default::default()));
	}
	match params.parse::<serde_json::Value>()? {
		input @ serde_json::Value::Object(_) => Ok(input),
		_ => Err(invalid_params(
			"Params must be passed by name, as an object",
		)),
	}
}

fn build_module(
	client: Arc<filen_sdk_rs::auth::Client>,
	endpoints: Vec<Arc<dyn AnyEndpoint>>,
	document: serde_json::Value,
) -> anyhow::Result<RpcModule<Arc<filen_sdk_rs::auth::Client>>> {
	let mut module = RpcModule::new(client);

	for endpoint in endpoints {
		module.register_async_method(endpoint.name(), move |params, client, _| {
			let endpoint = endpoint.clone();
			async move {
				let input = input_from_params(&params)?;
				endpoint.handle(&client, input).await.map_err(|error| {
					// `AnyEndpoint::handle` deserializes the input itself, so a JSON error
					// means the params didn't match the input schema.
					if error.downcast_ref::<serde_json::Error>().is_some() {
						invalid_params(error.to_string())
					} else {
						match UI::extract_failure(error) {
							Ok(failure) => ErrorObjectOwned::owned(
								ENDPOINT_ERROR_CODE,
								failure.to_string(),
								None::<()>,
							),
							Err(error) => ErrorObjectOwned::owned(
								ENDPOINT_ERROR_CODE,
								format!("{error:#}"),
								None::<()>,
							),
						}
					}
				})
			}
		})?;
	}

	// The spec's service discovery method.
	module.register_method("rpc.discover", move |_, _, _| {
		Ok::<_, ErrorObjectOwned>(document.clone())
	})?;

	Ok(module)
}

pub(crate) async fn run_json_rpc_server(
	client: Arc<filen_sdk_rs::auth::Client>,
	addr: Option<SocketAddr>,
	mut print_addr: impl FnMut(SocketAddr),
) -> anyhow::Result<()> {
	let server = Server::builder()
		.build(addr.unwrap_or("127.0.0.1:0".parse::<SocketAddr>()?))
		.await?;
	let addr = server.local_addr()?;

	let endpoints: Vec<Arc<dyn AnyEndpoint>> = all_endpoints().into_iter().map(Arc::from).collect();
	let document = serde_json::to_value(build_openrpc_document(&endpoints, addr))?;
	let module = build_module(client, endpoints, document)?;

	print_addr(addr);

	let handle = server.start(module);

	let mut stop_rx = crate::CTRLC_TX.subscribe();
	select! {
		_ = handle.clone().stopped() => {}
		_ = stop_rx.recv() => {
			handle.stop().unwrap_or_else(|error| {
				eprintln!("Failed to stop server: {}", error);
			});
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn document() -> OpenRPC {
		let endpoints: Vec<Arc<dyn AnyEndpoint>> =
			all_endpoints().into_iter().map(Arc::from).collect();
		build_openrpc_document(&endpoints, "127.0.0.1:1234".parse().unwrap())
	}

	fn collect_refs(value: &serde_json::Value, refs: &mut Vec<String>) {
		match value {
			serde_json::Value::Object(object) => {
				if let Some(serde_json::Value::String(reference)) = object.get("$ref") {
					refs.push(reference.clone());
				}
				object.values().for_each(|value| collect_refs(value, refs));
			}
			serde_json::Value::Array(array) => {
				array.iter().for_each(|value| collect_refs(value, refs))
			}
			_ => {}
		}
	}

	#[test]
	fn nullable_types_become_any_of() {
		let mut schema = schemars::json_schema!({
			"description": "Size in bytes",
			"type": ["integer", "null"],
			"format": "uint64",
			"minimum": 0,
		});
		nullable_type_to_any_of(&mut schema);
		assert_eq!(
			schema,
			schemars::json_schema!({
				"description": "Size in bytes",
				"anyOf": [
					{ "type": "integer", "format": "uint64", "minimum": 0 },
					{ "type": "null" },
				],
			})
		);
	}

	#[test]
	fn document_has_no_type_lists() {
		fn check(value: &serde_json::Value) {
			match value {
				serde_json::Value::Object(object) => {
					assert!(
						!object.get("type").is_some_and(|ty| ty.is_array()),
						"{value}"
					);
					object.values().for_each(check);
				}
				serde_json::Value::Array(array) => array.iter().for_each(check),
				_ => {}
			}
		}
		check(&serde_json::to_value(document()).unwrap());
	}

	#[test]
	fn document_round_trips() {
		let document = document();
		let json = serde_json::to_value(&document).unwrap();
		let parsed: OpenRPC = serde_json::from_value(json).unwrap();
		assert_eq!(parsed, document);
	}

	#[test]
	fn document_has_one_by_name_method_per_endpoint() {
		let document = document();
		let mut method_names = Vec::new();
		for method in &document.methods {
			let ReferenceOr::Item(method) = method else {
				panic!("methods are inlined");
			};
			assert_eq!(method.param_structure, Some(ParamStructure::ByName));
			// Optional params must not precede required ones.
			let required: Vec<bool> = method
				.params
				.iter()
				.map(|param| match param {
					ReferenceOr::Item(param) => param.required == Some(true),
					ReferenceOr::Reference(_) => panic!("params are inlined"),
				})
				.collect();
			assert!(
				required.is_sorted_by(|a, b| a >= b),
				"{}: {required:?}",
				method.name
			);
			method_names.push(method.name.clone());
		}
		let mut endpoint_names: Vec<String> = all_endpoints()
			.iter()
			.map(|endpoint| endpoint.name().to_string())
			.collect();
		method_names.sort();
		endpoint_names.sort();
		assert_eq!(method_names, endpoint_names);
	}

	#[test]
	fn document_refs_resolve() {
		let document = document();
		let components = document.components.as_ref().unwrap();
		let mut refs = Vec::new();
		collect_refs(&serde_json::to_value(&document).unwrap(), &mut refs);
		assert!(!refs.is_empty());
		for reference in refs {
			let resolves = if let Some(name) = reference.strip_prefix("#/components/schemas/") {
				components.schemas.as_ref().unwrap().contains_key(name)
			} else if let Some(name) = reference.strip_prefix("#/components/errors/") {
				components.errors.as_ref().unwrap().contains_key(name)
			} else {
				false
			};
			assert!(resolves, "dangling reference {reference}");
		}
	}
}
