use super::*;
use crate::test_support::compact;

fn expand(item: ItemImpl) -> String {
	compact(js_exports_tokens(item).unwrap())
}

fn error(item: ItemImpl) -> String {
	js_exports_tokens(item).unwrap_err().to_string()
}

#[test]
fn a_block_without_wasm_bindgen_passes_through_unchanged() {
	let item: ItemImpl = syn::parse_quote! {
		#[uniffi::export]
		impl JsClient {
			pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
				self.inner_get_dir(uuid).await
			}
		}
	};
	assert_eq!(expand(item.clone()), compact(item));
}

#[test]
fn an_async_export_reads_its_params_in_a_wrapper_and_the_original_moves_out() {
	let item: ItemImpl = syn::parse_quote! {
		/// The client.
		#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
		impl JsClient {
			pub fn root(&self) -> Dir {
				self.inner_root()
			}

			/// Gets a dir.
			#[wasm_bindgen::prelude::wasm_bindgen(js_name = "getDir")]
			pub async fn get_dir(&self, uuid: UuidStr, color: Option<DirColor>) -> Result<Dir, Error> {
				self.inner_get_dir(uuid, color).await
			}

			#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listRoot")]
			pub async fn list_root(&self) -> Result<Vec<Dir>, Error> {
				self.inner_list_root().await
			}

			pub(crate) async fn trash(&self, dir: Dir) -> Result<(), Error> {
				self.inner_trash(dir).await
			}
		}
	};
	let expected = quote! {
		/// The client.
		#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
		impl JsClient {
			pub fn root(&self) -> Dir {
				self.inner_root()
			}

			/// Gets a dir.
			#[wasm_bindgen::prelude::wasm_bindgen(js_name = "getDir")]
			pub async fn js_export_get_dir(
				&self,
				uuid: <UuidStr as crate::js::JsParam>::Wire,
				color: <Option<DirColor> as crate::js::JsParam>::Wire
			) -> Result<Dir, Error> {
				let uuid = <UuidStr as crate::js::JsParam>::parse(uuid)?;
				let color = <Option<DirColor> as crate::js::JsParam>::parse(color)?;
				self.get_dir(uuid, color).await
			}

			#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listRoot")]
			pub async fn list_root(&self) -> Result<Vec<Dir>, Error> {
				self.inner_list_root().await
			}

			pub(crate) async fn trash(&self, dir: Dir) -> Result<(), Error> {
				self.inner_trash(dir).await
			}
		}

		/// The client.
		impl JsClient {
			/// Gets a dir.
			pub async fn get_dir(&self, uuid: UuidStr, color: Option<DirColor>) -> Result<Dir, Error> {
				self.inner_get_dir(uuid, color).await
			}
		}
	};
	assert_eq!(expand(item), compact(expected));
}

#[test]
fn an_export_without_a_js_name_is_given_its_rust_name() {
	let item: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "UnauthClient")]
		impl UnauthJsClient {
			pub async fn login(&self, params: LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}
	};
	let expected = quote! {
		#[wasm_bindgen(js_class = "UnauthClient")]
		impl UnauthJsClient {
			#[wasm_bindgen::prelude::wasm_bindgen(js_name = "login")]
			pub async fn js_export_login(
				&self,
				params: <LoginParams as crate::js::JsParam>::Wire
			) -> Result<JsClient, Error> {
				let params = <LoginParams as crate::js::JsParam>::parse(params)?;
				self.login(params).await
			}
		}

		impl UnauthJsClient {
			pub async fn login(&self, params: LoginParams) -> Result<JsClient, Error> {
				self.inner_login(params).await
			}
		}
	};
	assert_eq!(expand(item), compact(expected));
}

#[test]
fn a_js_name_behind_a_cfg_attr_stays_on_the_wrapper_alone() {
	let item: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			#[cfg_attr(
				all(target_family = "wasm", target_os = "unknown"),
				wasm_bindgen(js_name = "deleteAccount")
			)]
			#[cfg_attr(feature = "uniffi", uniffi::method(name = "delete_account"))]
			pub async fn delete_account(&self, two_factor_code: Option<String>) -> Result<(), Error> {
				self.inner_delete_account(two_factor_code).await
			}
		}
	};
	let expected = quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			#[cfg_attr(
				all(target_family = "wasm", target_os = "unknown"),
				wasm_bindgen(js_name = "deleteAccount")
			)]
			#[cfg_attr(feature = "uniffi", uniffi::method(name = "delete_account"))]
			pub async fn js_export_delete_account(
				&self,
				two_factor_code: <Option<String> as crate::js::JsParam>::Wire
			) -> Result<(), Error> {
				let two_factor_code = <Option<String> as crate::js::JsParam>::parse(two_factor_code)?;
				self.delete_account(two_factor_code).await
			}
		}

		impl JsClient {
			#[cfg_attr(feature = "uniffi", uniffi::method(name = "delete_account"))]
			pub async fn delete_account(&self, two_factor_code: Option<String>) -> Result<(), Error> {
				self.inner_delete_account(two_factor_code).await
			}
		}
	};
	assert_eq!(expand(item), compact(expected));
}

#[test]
fn a_params_attributes_stay_on_the_wrapper_and_its_mut_on_the_original() {
	let item: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			#[wasm_bindgen(js_name = "listDirRecursive")]
			pub async fn list_dir_recursive(
				&self,
				mut dir: AnyDirWithContext,
				#[wasm_bindgen(unchecked_param_type = "() => void")]
				callback: web_sys::js_sys::Function,
			) -> Result<DirsAndFiles, Error> {
				dir.prepare();
				self.inner_list_dir_recursive(dir, callback).await
			}
		}
	};
	let expected = quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			#[wasm_bindgen(js_name = "listDirRecursive")]
			pub async fn js_export_list_dir_recursive(
				&self,
				dir: <AnyDirWithContext as crate::js::JsParam>::Wire,
				#[wasm_bindgen(unchecked_param_type = "() => void")]
				callback: <web_sys::js_sys::Function as crate::js::JsParam>::Wire,
			) -> Result<DirsAndFiles, Error> {
				let dir = <AnyDirWithContext as crate::js::JsParam>::parse(dir)?;
				let callback = <web_sys::js_sys::Function as crate::js::JsParam>::parse(callback)?;
				self.list_dir_recursive(dir, callback).await
			}
		}

		impl JsClient {
			pub async fn list_dir_recursive(
				&self,
				mut dir: AnyDirWithContext,
				callback: web_sys::js_sys::Function,
			) -> Result<DirsAndFiles, Error> {
				dir.prepare();
				self.inner_list_dir_recursive(dir, callback).await
			}
		}
	};
	assert_eq!(expand(item), compact(expected));
}

#[test]
fn a_static_export_calls_the_original_through_self() {
	let item: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			#[wasm_bindgen(js_name = "fromStringified")]
			pub async fn from_stringified(client: StringifiedClient) -> Result<JsClient, Error> {
				Self::inner_from_stringified(client).await
			}
		}
	};
	let tokens = expand(item);
	assert!(
		tokens.contains(&compact(quote! {
			pub async fn js_export_from_stringified(
				client: <StringifiedClient as crate::js::JsParam>::Wire
			) -> Result<JsClient, Error> {
				let client = <StringifiedClient as crate::js::JsParam>::parse(client)?;
				Self::from_stringified(client).await
			}
		})),
		"{tokens}"
	);
}

#[test]
fn a_block_wasm_bindgen_has_already_expanded_is_an_error() {
	let item: ItemImpl = syn::parse_quote! {
		impl JsClient {
			#[wasm_bindgen::prelude::__wasm_bindgen_class_marker(
				JsClient = "Client",
				wasm_bindgen = wasm_bindgen,
				wasm_bindgen_futures = wasm_bindgen_futures
			)]
			pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
				self.inner_get_dir(uuid).await
			}
		}
	};
	assert_eq!(
		error(item),
		"list #[js_exports] before #[wasm_bindgen], which has already expanded this block"
	);
}

#[test]
fn an_unexpanded_cfg_attr_holding_wasm_bindgen_is_an_error() {
	let item: ItemImpl = syn::parse_quote! {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen(js_class = "Client")
		)]
		impl JsClient {
			pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
				self.inner_get_dir(uuid).await
			}
		}
	};
	assert_eq!(
		error(item),
		"#[js_exports] cannot tell whether this cfg_attr exports the block"
	);
}

#[test]
fn a_param_that_is_not_a_plain_name_is_an_error() {
	let item: ItemImpl = syn::parse_quote! {
		#[wasm_bindgen(js_class = "Client")]
		impl JsClient {
			pub async fn resize(&self, (width, height): (u32, u32)) -> Result<(), Error> {
				self.inner_resize(width, height).await
			}
		}
	};
	assert_eq!(
		error(item),
		"#[js_exports] needs each param of an async export to be a plain name"
	);
}

fn js_type_state(args: proc_macro2::TokenStream) -> JsTypeState {
	let args =
		syn::parse::Parser::parse2(Punctuated::<Meta, Comma>::parse_terminated, args).unwrap();
	parse_args(args.into_iter().collect()).unwrap()
}

fn js_param_impl_of(name: &str) -> String {
	let name = format_ident!("{}", name);
	compact(quote! {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		impl crate::js::JsParam for #name {
			type Wire = <Self as tsify::Tsify>::JsType;

			fn parse(wire: Self::Wire) -> ::std::result::Result<Self, crate::Error> {
				crate::js::parse_tsify(wire)
			}
		}
	})
}

#[test]
fn an_imported_struct_is_read_through_js_param() {
	let item: ItemStruct = syn::parse_quote! {
		pub struct LoginParams {
			pub email: String,
		}
	};
	let tokens = compact(parse_struct(item, js_type_state(quote!(import, wasm_all))));
	assert!(
		tokens.ends_with(&js_param_impl_of("LoginParams")),
		"{tokens}"
	);
}

#[test]
fn an_imported_enum_is_read_through_js_param() {
	let item: ItemEnum = syn::parse_quote! {
		pub enum SharingRole {
			Sharer(u64),
			Receiver(u64),
		}
	};
	let tokens = compact(enum_tokens(
		item,
		js_type_state(quote!(import, export, wasm_all)),
	));
	assert!(
		tokens.ends_with(&js_param_impl_of("SharingRole")),
		"{tokens}"
	);
}

#[test]
fn an_export_only_type_is_not_a_js_param() {
	let item: ItemStruct = syn::parse_quote! {
		pub struct CopyReport {
			pub copied: u64,
		}
	};
	let tokens = compact(parse_struct(item, js_type_state(quote!(export, wasm_all))));
	assert!(!tokens.contains("JsParam"), "{tokens}");
}
