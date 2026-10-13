use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

pub(crate) mod copy_item;
pub(crate) mod create_directory;
pub(crate) mod delete_item;
pub(crate) mod delete_public_link;
pub(crate) mod get_public_link;
pub(crate) mod list_public_links;
pub(crate) mod list_trash;
pub(crate) mod move_item;
pub(crate) mod public_link_expiration;
pub(crate) mod read_directory;
pub(crate) mod read_directory_recursive;
pub(crate) mod read_file;
pub(crate) mod restore_trash_item;
pub(crate) mod set_public_link;
pub(crate) mod stat;
pub(crate) mod util;
pub(crate) mod who_am_i;
pub(crate) mod write_file;

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
	/// Generates the input schema using `generator`, so that several endpoints can share one
	/// generator (and therefore one set of definitions and one `$ref` location).
	fn input_schema_with(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
		generator.root_schema_for::<Self::Input>()
	}
	fn output_schema_with(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
		generator.root_schema_for::<Self::Output>()
	}
	fn input_schema() -> schemars::Schema {
		Self::input_schema_with(&mut schemars::SchemaGenerator::default())
	}
	fn output_schema() -> schemars::Schema {
		Self::output_schema_with(&mut schemars::SchemaGenerator::default())
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
	fn input_schema_with(&self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema;
	fn output_schema_with(&self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema;
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

	fn input_schema_with(&self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
		<T as Endpoint>::input_schema_with(generator)
	}

	fn output_schema_with(&self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
		<T as Endpoint>::output_schema_with(generator)
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
	vec![
		Box::new(read_directory::ReadDirectory),
		Box::new(read_directory_recursive::ReadDirectoryRecursive),
		Box::new(read_file::ReadFile),
		Box::new(stat::Stat),
		Box::new(create_directory::CreateDirectory),
		Box::new(write_file::WriteFile),
		Box::new(move_item::MoveItem),
		Box::new(copy_item::CopyItem),
		Box::new(delete_item::DeleteItem),
		Box::new(list_trash::ListTrash),
		Box::new(restore_trash_item::RestoreTrashItem),
		Box::new(list_public_links::ListPublicLinks),
		Box::new(get_public_link::GetPublicLink),
		Box::new(set_public_link::SetPublicLink),
		Box::new(delete_public_link::DeletePublicLink),
		Box::new(who_am_i::WhoAmI),
	]
}
