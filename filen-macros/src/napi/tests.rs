use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{Item, ItemImpl, Meta};

use super::{Directions, mirror};
use crate::test_support::compact;
use crate::{js_exports_tokens, js_type_tokens, parse_args};

fn js_type(args: TokenStream2, item: Item) -> String {
	let args = syn::parse::Parser::parse2(Punctuated::<Meta, Comma>::parse_terminated, args)
		.unwrap()
		.into_iter()
		.collect();
	compact(js_type_tokens(item, parse_args(args).unwrap()).unwrap())
}

fn mirror_of(import: bool, export: bool, item: Item) -> String {
	compact(mirror(&item, Directions { import, export }).unwrap())
}

fn mirror_error(item: Item) -> String {
	mirror(
		&item,
		Directions {
			import: true,
			export: true,
		},
	)
	.unwrap_err()
	.to_string()
}

fn exports(item: ItemImpl) -> String {
	compact(js_exports_tokens(item).unwrap())
}

#[test]
fn the_napi_flag_only_appends_the_mirror_to_what_js_type_writes() {
	let item: Item = syn::parse_quote! {
		pub struct LoginParams {
			pub email: String,
		}
	};
	let without = js_type(quote!(import), item.clone());
	let with = js_type(quote!(import, napi), item);
	assert!(!without.contains("napi"), "{without}");
	let mirror = with.strip_prefix(&without).expect(&with);
	assert!(
		mirror.starts_with(
			r#"#[cfg(all(feature="napi"))]#[allow(unsafe_code)]mod__napi_login_params{"#
		),
		"{mirror}"
	);
}

#[test]
fn an_imported_record_reads_each_field_through_its_wire_type() {
	let item: Item = syn::parse_quote! {
		/// How to log in.
		pub struct LoginParams {
			/// The account's email.
			pub email: String,
			pub two_factor_code: Option<String>,
		}
	};
	let expected = quote! {
		#[cfg(all(feature = "napi"))]
		#[allow(unsafe_code)]
		mod __napi_login_params {
			use super::*;

			#[::napi_derive::napi(object, js_name = "LoginParams", object_to_js = false)]
			#[doc = r" How to log in."]
			pub struct LoginParamsNapi {
				#[doc = r" The account's email."]
				#[napi(js_name = "email", ts_type = "string")]
				pub email: <String as crate::js::napi::Wire>::Wire,
				#[napi(js_name = "twoFactorCode", ts_type = "string | undefined")]
				pub two_factor_code: ::std::option::Option<<String as crate::js::napi::Wire>::Wire>,
			}

			impl crate::js::napi::Wire for LoginParams {
				type Wire = LoginParamsNapi;
			}

			impl crate::js::napi::FromWire for LoginParams {
				fn from_wire(wire: LoginParamsNapi) -> ::std::result::Result<Self, crate::Error> {
					::std::result::Result::Ok(Self {
						email: <String as crate::js::napi::FromWire>::from_wire(wire.email)?,
						two_factor_code: <Option<String> as crate::js::napi::FromWire>::from_wire(
							wire.two_factor_code
						)?,
					})
				}
			}
		}
	};
	assert_eq!(mirror_of(true, false, item), compact(expected));
}

#[test]
fn an_exported_record_writes_each_field_and_is_never_read() {
	let item: Item = syn::parse_quote! {
		pub struct Report {
			pub copied: u32,
		}
	};
	let tokens = mirror_of(false, true, item);
	assert!(
		tokens.contains(r#"#[::napi_derive::napi(object,js_name="Report",object_from_js=false)]"#),
		"{tokens}"
	);
	assert!(
		tokens.contains(&compact(quote! {
			impl crate::js::napi::IntoWire for Report {
				fn into_wire(self) -> ReportNapi {
					ReportNapi {
						copied: <u32 as crate::js::napi::IntoWire>::into_wire(self.copied),
					}
				}
			}
		})),
		"{tokens}"
	);
	assert!(!tokens.contains("FromWire"), "{tokens}");
}

#[test]
fn a_record_read_and_written_converts_both_ways() {
	let item: Item = syn::parse_quote! {
		pub struct StringifiedClient {
			pub user_id: u64,
		}
	};
	let tokens = mirror_of(true, true, item);
	assert!(
		tokens.contains(r#"#[::napi_derive::napi(object,js_name="StringifiedClient")]"#),
		"{tokens}"
	);
	assert!(tokens.contains("implcrate::js::napi::FromWireforStringifiedClient"));
	assert!(tokens.contains("implcrate::js::napi::IntoWireforStringifiedClient"));
}

#[test]
fn an_i64_field_is_wired_as_its_wire_type_and_typed_bigint() {
	// napi itself would read and print a raw i64 as a number
	let item: Item = syn::parse_quote! {
		pub struct Offset {
			pub millis: i64,
		}
	};
	let tokens = mirror_of(true, true, item);
	assert!(
		tokens.contains(&compact(quote! {
			#[napi(js_name = "millis", ts_type = "bigint")]
			pub millis: <i64 as crate::js::napi::Wire>::Wire,
		})),
		"{tokens}"
	);
}

#[test]
fn an_option_field_is_a_literal_option_around_the_wire_type() {
	// napi-derive tells an optional field by its type's literal `Option` segment
	let item: Item = syn::parse_quote! {
		pub struct Progress {
			pub total_bytes: Option<u64>,
			pub roots: Option<Vec<Uuid>>,
		}
	};
	let tokens = mirror_of(false, true, item);
	assert!(
		tokens.contains(&compact(quote! {
			#[napi(js_name = "totalBytes", ts_type = "bigint | undefined")]
			pub total_bytes: ::std::option::Option<<u64 as crate::js::napi::Wire>::Wire>,
			#[napi(js_name = "roots", ts_type = "Array<string> | undefined")]
			pub roots: ::std::option::Option<::std::vec::Vec<<Uuid as crate::js::napi::Wire>::Wire>>,
		})),
		"{tokens}"
	);
	assert!(!tokens.contains("asOptionascrate"), "{tokens}");
	assert!(
		!tokens.contains("<Option<u64>ascrate::js::napi::Wire>"),
		"{tokens}"
	);
}

#[test]
fn a_field_without_a_ts_rule_is_an_error() {
	let item: Item = syn::parse_quote! {
		pub struct Thumb {
			pub scale: f32,
		}
	};
	assert!(mirror_error(item).contains("`f32` has no napi rule"));
}

#[test]
fn a_unit_enum_is_ubrns_numeric_enum_with_its_variants_in_upper_camel_case() {
	let item: Item = syn::parse_quote! {
		#[derive(Default)]
		pub enum LogLevel {
			Off,
			#[default]
			Info,
			/// Input and output.
			IO,
		}
	};
	let expected = quote! {
		#[cfg(all(feature = "napi"))]
		#[allow(unsafe_code)]
		mod __napi_log_level {
			use super::*;

			#[::napi_derive::napi(js_name = "LogLevel")]
			#[allow(clippy::enum_variant_names)]
			pub enum LogLevelNapi {
				Off,
				Info,
				#[doc = r" Input and output."]
				Io,
			}

			impl crate::js::napi::Wire for LogLevel {
				type Wire = LogLevelNapi;
			}

			impl crate::js::napi::FromWire for LogLevel {
				fn from_wire(wire: LogLevelNapi) -> ::std::result::Result<Self, crate::Error> {
					::std::result::Result::Ok(match wire {
						LogLevelNapi::Off => Self::Off,
						LogLevelNapi::Info => Self::Info,
						LogLevelNapi::Io => Self::IO,
					})
				}
			}

			impl crate::js::napi::IntoWire for LogLevel {
				fn into_wire(self) -> LogLevelNapi {
					match self {
						Self::Off => LogLevelNapi::Off,
						Self::Info => LogLevelNapi::Info,
						Self::IO => LogLevelNapi::Io,
					}
				}
			}
		}
	};
	assert_eq!(mirror_of(true, true, item), compact(expected));
}

#[test]
fn a_data_enum_is_a_tag_union_with_inner_payloads_and_a_tags_enum() {
	let item: Item = syn::parse_quote! {
		pub enum ResyncProgressMessage {
			Listing { root_index: u64, total_bytes: Option<u64> },
			Applying,
		}
	};
	let expected = quote! {
		#[cfg(all(feature = "napi"))]
		#[allow(unsafe_code)]
		mod __napi_resync_progress_message {
			use super::*;

			#[::napi_derive::napi(
				object,
				js_name = "ResyncProgressMessage_Listing_Inner",
				object_from_js = false
			)]
			pub struct ResyncProgressMessageListingInner {
				#[napi(js_name = "rootIndex", ts_type = "bigint")]
				pub root_index: <u64 as crate::js::napi::Wire>::Wire,
				#[napi(js_name = "totalBytes", ts_type = "bigint | undefined")]
				pub total_bytes: ::std::option::Option<<u64 as crate::js::napi::Wire>::Wire>,
			}

			#[::napi_derive::napi(
				discriminant = "tag",
				js_name = "ResyncProgressMessage",
				object_from_js = false
			)]
			#[allow(clippy::enum_variant_names)]
			pub enum ResyncProgressMessageNapi {
				Listing {
					#[napi(ts_type = "ResyncProgressMessage_Listing_Inner")]
					inner: ResyncProgressMessageListingInner,
				},
				Applying,
			}

			#[::napi_derive::napi(string_enum, js_name = "ResyncProgressMessage_Tags")]
			#[allow(clippy::enum_variant_names)]
			pub enum ResyncProgressMessageTags {
				Listing,
				Applying,
			}

			impl crate::js::napi::Wire for ResyncProgressMessage {
				type Wire = ResyncProgressMessageNapi;
			}

			impl crate::js::napi::IntoWire for ResyncProgressMessage {
				fn into_wire(self) -> ResyncProgressMessageNapi {
					match self {
						Self::Listing { root_index, total_bytes, } => ResyncProgressMessageNapi::Listing {
							inner: ResyncProgressMessageListingInner {
								root_index: <u64 as crate::js::napi::IntoWire>::into_wire(root_index),
								total_bytes: <Option<u64> as crate::js::napi::IntoWire>::into_wire(
									total_bytes
								),
							},
						},
						Self::Applying => ResyncProgressMessageNapi::Applying,
					}
				}
			}
		}
	};
	assert_eq!(mirror_of(false, true, item), compact(expected));
}

#[test]
fn an_imported_data_enum_reads_each_variants_inner_payload() {
	let item: Item = syn::parse_quote! {
		pub enum SharingRole {
			Sharer { id: u64 },
		}
	};
	let tokens = mirror_of(true, false, item);
	assert!(
		tokens.contains(&compact(quote! {
			SharingRoleNapi::Sharer { inner } => Self::Sharer {
				id: <u64 as crate::js::napi::FromWire>::from_wire(inner.id)?,
			}
		})),
		"{tokens}"
	);
	assert!(tokens.contains("object_to_js=false"), "{tokens}");
}

#[test]
fn a_mirror_is_written_once_per_combination_of_its_members_cfgs() {
	let item: Item = syn::parse_quote! {
		pub struct JsClientConfig {
			pub concurrency: Option<u32>,
			#[cfg(feature = "archive")]
			pub archive_job_concurrency: Option<u32>,
		}
	};
	let tokens = mirror_of(true, false, item);
	let (without, with) = tokens
		.split_once(r#"#[cfg(all(feature="napi",feature="archive"))]"#)
		.expect(&tokens);
	assert!(
		without.starts_with(r#"#[cfg(all(feature="napi",not(feature="archive")))]"#),
		"{without}"
	);
	assert!(!without.contains("archive_job_concurrency"), "{without}");
	assert!(with.contains("pubarchive_job_concurrency:"), "{with}");
	assert!(!with.contains("#[cfg"), "{with}");
}

#[test]
fn a_cfgd_variant_is_kept_only_where_its_cfg_holds() {
	let item: Item = syn::parse_quote! {
		pub enum ErrorKind {
			Server,
			#[cfg(feature = "heif-decoder")]
			HeifError,
		}
	};
	let tokens = mirror_of(false, true, item);
	let (without, with) = tokens
		.split_once(r#"#[cfg(all(feature="napi",feature="heif-decoder"))]"#)
		.expect(&tokens);
	assert!(!without.contains("HeifError"), "{without}");
	assert!(
		with.contains("Self::HeifError=>ErrorKindNapi::HeifError"),
		"{with}"
	);
}

#[test]
fn shapes_without_a_napi_rule_are_errors() {
	assert!(
		mirror_error(syn::parse_quote!(
			pub enum Role {
				Sharer(u64),
			}
		))
		.contains("tuple variants have no napi rule")
	);
	assert!(
		mirror_error(syn::parse_quote!(
			pub enum Code {
				A = 1,
			}
		))
		.contains("explicit discriminants")
	);
	assert!(
		mirror_error(syn::parse_quote!(
			pub struct Pair(u32, u32);
		))
		.contains("named fields")
	);
	assert!(
		mirror_error(syn::parse_quote!(
			pub struct Page<'a> {
				pub name: &'a str,
			}
		))
		.contains("generic types")
	);
}

#[test]
fn napi_mirror_takes_import_and_export_only() {
	let args = syn::parse::Parser::parse2(
		Punctuated::<Meta, Comma>::parse_terminated,
		quote!(export, wasm_all),
	)
	.unwrap();
	assert_eq!(
		Directions::parse(args).err().unwrap().to_string(),
		"#[napi_mirror] takes `import`, `export` or both"
	);
}

#[test]
fn a_block_without_markers_gets_no_napi_module() {
	let item: ItemImpl = syn::parse_quote! {
		#[uniffi::export]
		impl JsClient {
			pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
				self.inner_get_dir(uuid).await
			}
		}
	};
	assert_eq!(exports(item.clone()), compact(item));
}

#[test]
fn markers_are_taken_off_on_every_build() {
	let uniffi: ItemImpl = syn::parse_quote! {
		#[uniffi::export]
		impl UnauthJsClient {
			#[napi_export]
			pub async fn login(&self, params: LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}
	};
	let tokens = exports(uniffi);
	assert!(!tokens.contains("napi_export]"), "{tokens}");
	assert!(
		tokens.starts_with(&compact(quote! {
			#[uniffi::export]
			impl UnauthJsClient {
				pub async fn login(&self, params: LoginParams) -> Result<JsClient, Error> {
					self.inner_login(params).await
				}
			}
		})),
		"{tokens}"
	);

	let wasm: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "UnauthClient")]
		impl UnauthJsClient {
			#[napi_export]
			pub async fn login(&self, params: LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}
	};
	let tokens = exports(wasm);
	assert!(!tokens.contains("napi_export]"), "{tokens}");
	assert!(tokens.contains("js_export_login"), "{tokens}");
}

#[test]
fn an_async_method_returns_a_promise_through_spawn_promise() {
	let item: ItemImpl = syn::parse_quote! {
		impl UnauthJsClient {
			/// Logs in.
			#[napi_export]
			pub async fn login(&self, params: crate::js::LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}
	};
	let expected = quote! {
		impl UnauthJsClient {
			/// Logs in.
			pub async fn login(&self, params: crate::js::LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}

		#[cfg(all(feature = "napi"))]
		mod __napi_export_unauth_js_client_login {
			use super::*;

			type __NapiParamParams = <crate::js::LoginParams as crate::js::napi::Wire>::Wire;
			type __NapiReturn = <JsClient as crate::js::napi::Wire>::Wire;

			fn __napi_call<'env>(
				this: &UnauthJsClient,
				env: &'env ::napi::Env,
				params: __NapiParamParams,
				async_opts: ::std::option::Option<::napi::bindgen_prelude::Object<'env>>,
			) -> ::napi::Result<::napi::bindgen_prelude::PromiseRaw<'env, __NapiReturn>> {
				let this = ::std::clone::Clone::clone(this);
				crate::js::napi::spawn_promise(env, async_opts, async move {
					let params = <crate::js::LoginParams as crate::js::napi::FromWire>::from_wire(params)?;
					this.login(params).await
				})
			}

			#[allow(unsafe_code)]
			mod __napi_class {
				use super::{__napi_call, __NapiParamParams, __NapiReturn};
				use crate::js::napi::UnauthJsClient;

				#[::napi_derive::napi]
				impl UnauthJsClient {
					#[doc = r" Logs in."]
					#[napi(
						js_name = "login",
						ts_args_type = "params: LoginParams, asyncOpts_?: { signal: AbortSignal }",
						ts_return_type = "Promise<JsClient>"
					)]
					pub fn __napi_login<'env>(
						&self,
						env: &'env ::napi::Env,
						params: __NapiParamParams,
						async_opts: ::std::option::Option<::napi::bindgen_prelude::Object<'env>>,
					) -> ::napi::Result<::napi::bindgen_prelude::PromiseRaw<'env, __NapiReturn>> {
						__napi_call(&self.0, env, params, async_opts)
					}
				}
			}
		}
	};
	assert_eq!(exports(item), compact(expected));
}

#[test]
fn an_infallible_async_method_resolves_with_its_value() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			pub async fn to_stringified(&self) -> StringifiedClient {
				self.inner_to_stringified().await
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			#[napi(
				js_name = "toStringified",
				ts_args_type = "asyncOpts_?: { signal: AbortSignal }",
				ts_return_type = "Promise<StringifiedClient>"
			)]
		})),
		"{tokens}"
	);
	assert!(
		tokens.contains(&compact(quote! {
			crate::js::napi::spawn_promise(env, async_opts, async move {
				::std::result::Result::<_, crate::Error>::Ok(this.to_stringified().await)
			})
		})),
		"{tokens}"
	);
}

#[test]
fn a_constructor_is_a_static_that_throws_its_error() {
	let item: ItemImpl = syn::parse_quote! {
		impl UnauthJsClient {
			#[napi_export]
			#[cfg_attr(feature = "uniffi", uniffi::constructor)]
			pub fn from_config(client_config: JsClientConfig) -> Result<Self, Error> {
				Self::inner_from_config(client_config)
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			#[napi(
				js_name = "fromConfig",
				ts_args_type = "clientConfig: JsClientConfig",
				ts_return_type = "UnauthJsClient"
			)]
			pub fn __napi_from_config(
				env: &::napi::Env,
				client_config: __NapiParamClientConfig,
			) -> ::napi::Result<__NapiReturn> {
				__napi_call(env, client_config,)
			}
		})),
		"{tokens}"
	);
	assert!(
		tokens.contains(&compact(quote! {
			type __NapiParamClientConfig = <JsClientConfig as crate::js::napi::Wire>::Wire;
			type __NapiReturn = <UnauthJsClient as crate::js::napi::Wire>::Wire;

			fn __napi_call(
				env: &::napi::Env,
				client_config: __NapiParamClientConfig,
			) -> ::napi::Result<__NapiReturn> {
				let client_config = <JsClientConfig as crate::js::napi::FromWire>::from_wire(client_config)
					.map_err(|e| crate::js::napi::throw(env, e))?;
				::std::result::Result::Ok(<UnauthJsClient as crate::js::napi::IntoWire>::into_wire(
					UnauthJsClient::from_config(client_config)
						.map_err(|e| crate::js::napi::throw(env, e))?
				))
			}
		})),
		"{tokens}"
	);
	// the uniffi attribute stays on the original only
	assert_eq!(tokens.matches("uniffi::constructor").count(), 1, "{tokens}");
}

#[test]
fn a_sync_method_calls_through_the_class_and_lends_a_borrowed_param() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			pub fn name_len(&self, name: &str) -> u32 {
				self.inner_name_len(name)
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			type __NapiParamName = <String as crate::js::napi::Wire>::Wire;
			type __NapiReturn = <u32 as crate::js::napi::Wire>::Wire;

			fn __napi_call(
				this: &JsClient,
				env: &::napi::Env,
				name: __NapiParamName,
			) -> ::napi::Result<__NapiReturn> {
				let name = <String as crate::js::napi::FromWire>::from_wire(name)
					.map_err(|e| crate::js::napi::throw(env, e))?;
				::std::result::Result::Ok(<u32 as crate::js::napi::IntoWire>::into_wire(
					this.name_len(&name)
				))
			}
		})),
		"{tokens}"
	);
	assert!(tokens.contains(r#"ts_args_type="name:string""#), "{tokens}");
}

#[test]
fn an_option_return_is_undefined_when_none() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			pub fn root(&self) -> Option<Dir> {
				self.inner_root()
			}

			#[napi_export]
			pub async fn find(&self, name: String) -> Result<Option<Dir>, Error> {
				self.inner_find(name).await
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			type __NapiReturn = <crate::js::napi::OptionReturn<Dir> as crate::js::napi::Wire>::Wire;

			fn __napi_call(this: &JsClient,) -> ::napi::Result<__NapiReturn> {
				::std::result::Result::Ok(
					<crate::js::napi::OptionReturn<Dir> as crate::js::napi::IntoWire>::into_wire(
						crate::js::napi::OptionReturn(this.root())
					)
				)
			}
		})),
		"{tokens}"
	);
	assert!(
		tokens.contains(&compact(quote! {
			async move {
				let name = <String as crate::js::napi::FromWire>::from_wire(name)?;
				::std::result::Result::map(this.find(name).await, crate::js::napi::OptionReturn)
			}
		})),
		"{tokens}"
	);
	assert!(
		tokens.contains(&compact(quote! {
			#[napi(js_name = "root", ts_args_type = "", ts_return_type = "Dir | undefined")]
			pub fn __napi_root(&self,) -> ::napi::Result<__NapiReturn> {
				__napi_call(&self.0,)
			}
		})),
		"{tokens}"
	);
	assert!(
		tokens.contains(r#"ts_return_type="Promise<Dir|undefined>""#),
		"{tokens}"
	);
}

#[test]
fn an_arc_receiver_gets_an_arc_of_the_inner_value() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			pub fn lock(self: std::sync::Arc<Self>) -> u32 {
				self.inner_lock()
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			let this = ::std::sync::Arc::new(::std::clone::Clone::clone(this));
			::std::result::Result::Ok(<u32 as crate::js::napi::IntoWire>::into_wire(this.lock()))
		})),
		"{tokens}"
	);
}

#[test]
fn each_marked_method_gets_its_own_module_behind_its_cfgs() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			#[cfg(not(target_family = "wasm"))]
			pub async fn set_limits(&self, upload_kbps: u32) {
				self.inner_set_limits(upload_kbps).await
			}

			#[napi_export]
			pub async fn flush(&self) {
				self.inner_flush().await
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(
			r#"#[cfg(all(feature="napi",not(target_family="wasm")))]mod__napi_export_js_client_set_limits"#
		),
		"{tokens}"
	);
	assert!(
		tokens.contains(r#"#[cfg(all(feature="napi"))]mod__napi_export_js_client_flush"#),
		"{tokens}"
	);
	assert!(
		tokens.contains(r#"ts_return_type="Promise<void>""#),
		"{tokens}"
	);
}

#[test]
fn a_param_without_a_ts_rule_is_an_error() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[napi_export]
			pub async fn upload(&self, bytes: Vec<u8>) -> Result<(), Error> {
				self.inner_upload(bytes).await
			}
		}
	};
	assert!(
		js_exports_tokens(item)
			.unwrap_err()
			.to_string()
			.contains("ArrayBuffer")
	);
}

#[test]
fn relative_paths_are_rebased_into_the_generated_module() {
	let item: ItemImpl = syn::parse_quote! {
		impl UnauthJsClient {
			#[napi_export]
			pub fn from_stringified(
				stringified: self::StringifiedClient,
			) -> Result<super::JsClient, Error> {
				Self::inner_from_stringified(stringified)
			}
		}
	};
	let tokens = exports(item);
	assert!(
		tokens.contains(&compact(quote! {
			type __NapiParamStringified = <super::StringifiedClient as crate::js::napi::Wire>::Wire;
			type __NapiReturn = <super::super::JsClient as crate::js::napi::Wire>::Wire;
		})),
		"{tokens}"
	);
	let record: Item = syn::parse_quote! {
		pub struct Login {
			pub client: super::StringifiedClient,
		}
	};
	let mirror = mirror_of(true, false, record);
	assert!(
		mirror.contains(&compact(
			quote!(pub client: <super::super::StringifiedClient as crate::js::napi::Wire>::Wire)
		)),
		"{mirror}"
	);
}
