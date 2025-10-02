use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::Item;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::token::Comma;
use syn::{Attribute, Block, FnArg, Ident, ItemFn, Type, parse_macro_input};

mod anchored_ref;

#[derive(PartialEq, Eq)]
enum SelfPrefix {
	None,
	Method,
	Static,
}

impl From<SelfPrefix> for proc_macro2::TokenStream {
	fn from(prefix: SelfPrefix) -> Self {
		match prefix {
			SelfPrefix::None => quote!(),
			SelfPrefix::Method => quote!(self.),
			SelfPrefix::Static => quote!(Self::),
		}
	}
}

trait ItemFnLike: Clone {
	fn asyncness(&self) -> &Option<syn::token::Async>;
	fn block(&mut self) -> &mut Block;
	fn ident(&self) -> &Ident;
	fn set_ident(&mut self, ident: Ident);
	fn attrs(&mut self) -> &mut Vec<Attribute>;
	fn inputs(&self) -> &Punctuated<FnArg, Comma>;
	fn mut_inputs(&mut self) -> &mut Punctuated<FnArg, Comma>;
	fn default_prefix() -> SelfPrefix;
}

impl ItemFnLike for ItemFn {
	fn asyncness(&self) -> &Option<syn::token::Async> {
		&self.sig.asyncness
	}

	fn block(&mut self) -> &mut Block {
		&mut self.block
	}

	fn ident(&self) -> &Ident {
		&self.sig.ident
	}

	fn set_ident(&mut self, ident: Ident) {
		self.sig.ident = ident;
	}

	fn attrs(&mut self) -> &mut Vec<Attribute> {
		&mut self.attrs
	}

	fn inputs(&self) -> &Punctuated<FnArg, Comma> {
		&self.sig.inputs
	}

	fn mut_inputs(&mut self) -> &mut Punctuated<FnArg, Comma> {
		&mut self.sig.inputs
	}

	fn default_prefix() -> SelfPrefix {
		SelfPrefix::None
	}
}

impl ItemFnLike for syn::ImplItemFn {
	fn asyncness(&self) -> &Option<syn::token::Async> {
		&self.sig.asyncness
	}

	fn block(&mut self) -> &mut Block {
		&mut self.block
	}

	fn ident(&self) -> &Ident {
		&self.sig.ident
	}

	fn set_ident(&mut self, ident: Ident) {
		self.sig.ident = ident;
	}

	fn attrs(&mut self) -> &mut Vec<Attribute> {
		&mut self.attrs
	}

	fn inputs(&self) -> &Punctuated<FnArg, Comma> {
		&self.sig.inputs
	}

	fn mut_inputs(&mut self) -> &mut Punctuated<FnArg, Comma> {
		&mut self.sig.inputs
	}

	fn default_prefix() -> SelfPrefix {
		SelfPrefix::Static
	}
}

#[cfg(feature = "tokio")]
fn get_function_body(
	fn_prefix: proc_macro2::TokenStream,
	original_fn_name: &Ident,
	args: &[proc_macro2::TokenStream],
) -> syn::Result<Block> {
	syn::parse2(quote! {
		{
			println!("{} called", stringify!(#original_fn_name));
			let time = chrono::Utc::now();

			let ret = crate::env::get_runtime().spawn(async move {
				let time = chrono::Utc::now();
				let ret = #fn_prefix #original_fn_name(#(#args),*).await;
				log::info!(
				"{} inner time: {}ms",
				stringify!(#original_fn_name),
					(chrono::Utc::now() - time).num_milliseconds()
				);
				ret
			}).await.unwrap();

			log::info!(
				"{} full time: {}ms",
				stringify!(#original_fn_name),
				(chrono::Utc::now() - time).num_milliseconds()
			);

			ret
		}
	})
}

#[cfg(not(feature = "tokio"))]
fn get_function_body(
	fn_prefix: proc_macro2::TokenStream,
	original_fn_name: &Ident,
	args: &[proc_macro2::TokenStream],
) -> syn::Result<Block> {
	syn::parse2(quote! {
		{
			let time = chrono::Utc::now();

			let ret = #fn_prefix #original_fn_name(#(#args),*).await;

			log::info!(
				"{} call time: {}ms",
				stringify!(#original_fn_name),
				(chrono::Utc::now() - time).num_milliseconds()
			);

			ret
		}
	})
}

fn get_wrapper_fn<T>(item: &mut T) -> Result<T, syn::Error>
where
	T: ItemFnLike,
{
	// Transform the function body
	if !item.asyncness().is_some() {
		return Err(syn::Error::new(
			item.ident().span(),
			"create_uniffi_wrapper can only be applied to async functions",
		));
	}
	let mut new_item = item.clone();

	// Name
	new_item.set_ident(format_ident!("uniffi_{}", item.ident()));

	let mut fn_prefix = T::default_prefix();
	// Signature + calling args
	let mut args: Vec<proc_macro2::TokenStream> = Vec::new();
	for (og_param, new_param) in item.inputs().iter().zip(new_item.mut_inputs().iter_mut()) {
		if let FnArg::Receiver(receiver) = og_param {
			if receiver.reference.is_none() {
				args.push(quote!(self));
				continue;
			}
			fn_prefix = SelfPrefix::Method;
			*new_param = syn::parse_quote!(self: std::sync::Arc<Self>);
		} else if let (FnArg::Typed(pat_type), FnArg::Typed(new_pat_type)) = (og_param, new_param) {
			let arg_name = &new_pat_type.pat;
			if let Type::Reference(ref type_ref) = *pat_type.ty {
				let inner_type = &type_ref.elem;
				if let Type::Path(type_path) = inner_type.as_ref()
					&& type_path
						.path
						.segments
						.first()
						.is_some_and(|s| s.ident == "str")
				{
					*new_pat_type.ty = syn::parse_quote!(String);
					args.push(quote!(#arg_name.as_ref()));
					continue;
				}
				*new_pat_type.ty = syn::parse_quote!(std::sync::Arc<#inner_type>);
				args.push(quote!(#arg_name.as_ref()));
			} else {
				args.push(quote!(#arg_name));
			}
		} else {
			unreachable!()
		}
	}

	// Attribute
	let uniffi_attrs = item
		.attrs()
		.iter()
		.enumerate()
		.filter_map(|(i, attr)| {
			if attr.path().segments.len() == 1 && attr.path().segments[0].ident == "uniffi" {
				Some(i)
			} else {
				None
			}
		})
		.collect::<Vec<_>>();

	for i in uniffi_attrs.into_iter().rev() {
		new_item.attrs().push(item.attrs().remove(i));
	}

	let original_fn_name_string = item.ident().to_string();
	if fn_prefix == SelfPrefix::None {
		new_item
			.attrs()
			.push(syn::parse_quote!(#[uniffi::export(name = #original_fn_name_string)]));
	} else if fn_prefix == SelfPrefix::Method {
		new_item
			.attrs()
			.push(syn::parse_quote!(#[uniffi::method(name = #original_fn_name_string)]));
	}

	// Body
	let fn_prefix: proc_macro2::TokenStream = fn_prefix.into();
	let original_fn_name = item.ident();
	*new_item.block() = get_function_body(fn_prefix, original_fn_name, &args)?;

	Ok(new_item)
}

#[proc_macro_attribute]
pub fn create_uniffi_wrapper(_attr: TokenStream, item: TokenStream) -> TokenStream {
	// Parse the input function
	let input_fn = parse_macro_input!(item as Item);

	match input_fn {
		Item::Fn(mut fn_item) => {
			let wrapper_fn = get_wrapper_fn(&mut fn_item);
			match wrapper_fn {
				Ok(wrapper_fn) => quote! { #fn_item #wrapper_fn }.into(),
				Err(e) => e.to_compile_error().into(),
			}
		}
		Item::Impl(mut item_impl) => {
			let mut items_to_add = Vec::new();
			for item in item_impl.items.iter_mut() {
				if let syn::ImplItem::Fn(method) = item {
					let wrapper_fn = get_wrapper_fn(method);
					match wrapper_fn {
						Ok(wrapper_fn) => items_to_add.push(wrapper_fn.into()),
						Err(e) => return e.to_compile_error().into(),
					}
				}
			}
			let mut new_item_impl = item_impl.clone();
			new_item_impl.items = items_to_add;
			new_item_impl.attrs.push(syn::parse_quote!(#[uniffi::export]));
			quote! { #item_impl #new_item_impl }.into()
		}
		input => syn::Error::new(
			input.span(),
			"create_uniffi_wrapper can only be applied to async functions or impl blocks with async functions",
		)
		.into_compile_error()
		.into(),
	}
}

#[proc_macro_attribute]
pub fn shared_test_runtime(_attr: TokenStream, input: TokenStream) -> TokenStream {
	let input_fn = parse_macro_input!(input as ItemFn);

	// Extract function components
	let fn_vis = &input_fn.vis;
	let fn_name = &input_fn.sig.ident;
	let fn_generics = &input_fn.sig.generics;
	let fn_inputs = &input_fn.sig.inputs;
	let fn_output = &input_fn.sig.output;
	let fn_block = &input_fn.block;

	// Remove async from the signature and wrap the body
	let result = quote! {
		#[test]
		#fn_vis fn #fn_name #fn_generics(#fn_inputs) #fn_output {
			test_utils::rt().block_on(async #fn_block)
		}
	};

	result.into()
}

/// Derive macro for TransmuteLifetime trait
///
/// Generates different implementations based on whether the struct has lifetime parameters:
/// - For types with lifetimes: implements for 'static version with transmute
/// - For types without lifetimes: implements identity operations
#[proc_macro_derive(AnchorableRef)]
pub fn derive_transmute_lifetime(input: TokenStream) -> TokenStream {
	anchored_ref::derive_transmute_lifetime(input)
}
