use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::token::Comma;
use syn::{
	Attribute, Block, Data, DeriveInput, Fields, FnArg, GenericParam, Ident, ItemFn, Lifetime,
	LifetimeParam, Type, TypePath, Variant, parse_macro_input,
};
use syn::{Item, WherePredicate};

mod anchored_ref;
mod sdk_type_derives;

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

fn get_function_body(
	fn_prefix: proc_macro2::TokenStream,
	original_fn_name: &Ident,
	args: &[proc_macro2::TokenStream],
) -> syn::Result<Block> {
	syn::parse2(quote! {
		{
			crate::env::get_runtime().spawn(async move {
				#fn_prefix #original_fn_name(#(#args),*).await
			}).await.unwrap()
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
	let fn_attrs = &input_fn.attrs;

	// Remove async from the signature and wrap the body
	let result = quote! {
		#[test]
		#(#fn_attrs)*
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

#[proc_macro_derive(CowHelpers)]
pub fn derive_cow_helpers(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);
	let name = &input.ident;
	if input.generics.lifetimes().next().is_none() {
		return syn::Error::new_spanned(
			name,
			"CowHelpers is pointless for types without lifetime parameters",
		)
		.to_compile_error()
		.into();
	}

	match &input.data {
		Data::Struct(data) => derive_for_struct(&input, name, &data.fields),
		Data::Enum(data) => derive_for_enum(&input, name, &data.variants),
		Data::Union(_) => syn::Error::new_spanned(name, "CowHelpers cannot be derived for unions")
			.to_compile_error()
			.into(),
	}
}

fn derive_for_struct(input: &DeriveInput, name: &syn::Ident, fields: &Fields) -> TokenStream {
	let fields = match fields {
		Fields::Named(fields) => &fields.named,
		Fields::Unnamed(fields) => &fields.unnamed,
		Fields::Unit => {
			return syn::Error::new_spanned(name, "CowHelpers cannot be derived for unit structs")
				.to_compile_error()
				.into();
		}
	};

	// Check if any field has a lifetime
	let has_lifetime_in_fields = fields.iter().any(|f| type_has_lifetime(&f.ty));

	// Find the lifetime parameter (assume first lifetime, or 'a if none exists)
	let lifetime = input
		.generics
		.lifetimes()
		.next()
		.map(|lt| lt.lifetime.clone())
		.unwrap_or_else(|| Lifetime::new("'a", proc_macro2::Span::call_site()));

	// Build generics for the impl
	let mut impl_generics = input.generics.clone();

	// Ensure we have the lifetime parameter if fields use lifetimes
	let has_lifetime = impl_generics.lifetimes().any(|lt| lt.lifetime == lifetime);
	if !has_lifetime && has_lifetime_in_fields {
		impl_generics.params.insert(
			0,
			GenericParam::Lifetime(LifetimeParam::new(lifetime.clone())),
		);
	}

	let (_, ty_generics, where_clause) = input.generics.split_for_impl();
	let (impl_generics, _, _) = impl_generics.split_for_impl();

	// Generate field conversions for as_borrowed_cow
	let as_borrowed_fields = fields.iter().enumerate().map(|(i, f)| {
		let has_lifetime = type_has_lifetime(&f.ty);

		if let Some(ident) = &f.ident {
			if has_lifetime {
				quote! { #ident: self.#ident.as_borrowed_cow() }
			} else {
				quote! { #ident: self.#ident.clone() }
			}
		} else {
			let index = syn::Index::from(i);
			if has_lifetime {
				quote! { self.#index.as_borrowed_cow() }
			} else {
				quote! { self.#index.clone() }
			}
		}
	});

	// Generate field conversions for into_owned_cow
	let into_owned_fields = fields.iter().enumerate().map(|(i, f)| {
		let has_lifetime = type_has_lifetime(&f.ty);

		if let Some(ident) = &f.ident {
			if has_lifetime {
				quote! { #ident: self.#ident.into_owned_cow() }
			} else {
				quote! { #ident: self.#ident }
			}
		} else {
			let index = syn::Index::from(i);
			if has_lifetime {
				quote! { self.#index.into_owned_cow() }
			} else {
				quote! { self.#index }
			}
		}
	});

	// Determine if struct uses named or unnamed fields
	let is_named = fields
		.iter()
		.next()
		.and_then(|f| f.ident.as_ref())
		.is_some();

	let borrowed_constructor = if is_named {
		quote! { #name { #(#as_borrowed_fields),* } }
	} else {
		quote! { #name ( #(#as_borrowed_fields),* ) }
	};

	let owned_constructor = if is_named {
		quote! { #name { #(#into_owned_fields),* } }
	} else {
		quote! { #name ( #(#into_owned_fields),* ) }
	};

	// Build the type with different lifetimes
	let borrowed_type = build_type_with_lifetime(input, &lifetime, "borrow");
	let static_type = build_type_with_lifetime(input, &lifetime, "static");

	// Add Clone bound to where clause if we're cloning owned fields
	let extended_where_clause =
		build_where_clause_with_clone(where_clause, fields.iter().map(|f| &f.ty));

	let expanded = quote! {
		impl #impl_generics CowHelpers for #name #ty_generics #extended_where_clause {
			type CowBorrowed<'borrow> = #borrowed_type
			where
				Self: 'borrow;

			type CowStatic = #static_type;

			#[inline]
			fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
			where
				Self: 'borrow,
			{
				#borrowed_constructor
			}

			#[inline]
			fn into_owned_cow(self) -> Self::CowStatic {
				#owned_constructor
			}
		}
	};

	TokenStream::from(expanded)
}

fn derive_for_enum(
	input: &DeriveInput,
	name: &syn::Ident,
	variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> TokenStream {
	if variants.is_empty() {
		return syn::Error::new_spanned(name, "CowHelpers cannot be derived for empty enums")
			.to_compile_error()
			.into();
	}

	// Check if any variant has a lifetime
	let has_lifetime_in_variants = variants.iter().any(|v| match &v.fields {
		Fields::Named(fields) => fields.named.iter().any(|f| type_has_lifetime(&f.ty)),
		Fields::Unnamed(fields) => fields.unnamed.iter().any(|f| type_has_lifetime(&f.ty)),
		Fields::Unit => false,
	});

	// Find the lifetime parameter (assume first lifetime, or 'a if none exists)
	let lifetime = input
		.generics
		.lifetimes()
		.next()
		.map(|lt| lt.lifetime.clone())
		.unwrap_or_else(|| Lifetime::new("'a", proc_macro2::Span::call_site()));

	// Build generics for the impl
	let mut impl_generics = input.generics.clone();

	// Ensure we have the lifetime parameter if fields use lifetimes
	let has_lifetime = impl_generics.lifetimes().any(|lt| lt.lifetime == lifetime);
	if !has_lifetime && has_lifetime_in_variants {
		impl_generics.params.insert(
			0,
			GenericParam::Lifetime(LifetimeParam::new(lifetime.clone())),
		);
	}

	let (_, ty_generics, where_clause) = input.generics.split_for_impl();
	let (impl_generics, _, _) = impl_generics.split_for_impl();

	// Generate match arms for as_borrowed_cow
	let as_borrowed_arms = variants.iter().map(|variant| {
		let variant_name = &variant.ident;
		match &variant.fields {
			Fields::Named(fields) => {
				let field_names: Vec<_> = fields.named.iter().map(|f| &f.ident).collect();
				let field_conversions = fields.named.iter().map(|f| {
					let ident = f.ident.as_ref().unwrap();
					if type_has_lifetime(&f.ty) {
						quote! { #ident: #ident.as_borrowed_cow() }
					} else {
						quote! { #ident: #ident.clone() }
					}
				});
				quote! {
					#name::#variant_name { #(#field_names),* } => #name::#variant_name { #(#field_conversions),* }
				}
			}
			Fields::Unnamed(fields) => {
				let field_bindings: Vec<_> = (0..fields.unnamed.len())
					.map(|i| {
						syn::Ident::new(&format!("field_{}", i), proc_macro2::Span::call_site())
					})
					.collect();
				let field_conversions =
					fields
						.unnamed
						.iter()
						.zip(&field_bindings)
						.map(|(f, binding)| {
							if type_has_lifetime(&f.ty) {
								quote! { #binding.as_borrowed_cow() }
							} else {
								quote! { #binding.clone() }
							}
						});
				quote! {
					#name::#variant_name(#(#field_bindings),*) => #name::#variant_name(#(#field_conversions),*)
				}
			}
			Fields::Unit => {
				quote! {
					#name::#variant_name => #name::#variant_name
				}
			}
		}
	});

	// Generate match arms for into_owned_cow
	let into_owned_arms = variants.iter().map(|variant| {
		let variant_name = &variant.ident;
		match &variant.fields {
			Fields::Named(fields) => {
				let field_names: Vec<_> = fields.named.iter().map(|f| &f.ident).collect();
				let field_conversions = fields.named.iter().map(|f| {
					let ident = f.ident.as_ref().unwrap();
					if type_has_lifetime(&f.ty) {
						quote! { #ident: #ident.into_owned_cow() }
					} else {
						quote! { #ident: #ident }
					}
				});
				quote! {
					#name::#variant_name { #(#field_names),* } => #name::#variant_name { #(#field_conversions),* }
				}
			}
			Fields::Unnamed(fields) => {
				let field_bindings: Vec<_> = (0..fields.unnamed.len())
					.map(|i| {
						syn::Ident::new(&format!("field_{}", i), proc_macro2::Span::call_site())
					})
					.collect();
				let field_conversions =
					fields
						.unnamed
						.iter()
						.zip(&field_bindings)
						.map(|(f, binding)| {
							if type_has_lifetime(&f.ty) {
								quote! { #binding.into_owned_cow() }
							} else {
								quote! { #binding }
							}
						});
				quote! {
					#name::#variant_name(#(#field_bindings),*) => #name::#variant_name(#(#field_conversions),*)
				}
			}
			Fields::Unit => {
				quote! {
					#name::#variant_name => #name::#variant_name
				}
			}
		}
	});

	// Build the type with different lifetimes
	let borrowed_type = build_type_with_lifetime(input, &lifetime, "borrow");
	let static_type = build_type_with_lifetime(input, &lifetime, "static");

	// Collect all field types for Clone bounds
	let all_field_types = variants.iter().flat_map(|v| match &v.fields {
		Fields::Named(fields) => fields.named.iter().map(|f| &f.ty).collect::<Vec<_>>(),
		Fields::Unnamed(fields) => fields.unnamed.iter().map(|f| &f.ty).collect::<Vec<_>>(),
		Fields::Unit => vec![],
	});

	// Add Clone bound to where clause if we're cloning owned fields
	let extended_where_clause = build_where_clause_with_clone(where_clause, all_field_types);

	let expanded = quote! {
		impl #impl_generics CowHelpers for #name #ty_generics #extended_where_clause {
			type CowBorrowed<'borrow> = #borrowed_type
			where
				Self: 'borrow;

			type CowStatic = #static_type;

			#[inline]
			fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
			where
				Self: 'borrow,
			{
				match self {
					#(#as_borrowed_arms),*
				}
			}

			#[inline]
			fn into_owned_cow(self) -> Self::CowStatic {
				match self {
					#(#into_owned_arms),*
				}
			}
		}
	};

	TokenStream::from(expanded)
}

fn build_type_with_lifetime(
	input: &DeriveInput,
	original_lifetime: &Lifetime,
	new_lifetime_str: &str,
) -> proc_macro2::TokenStream {
	let name = &input.ident;
	let mut generics = input.generics.clone();

	// Replace the lifetime with the new one
	for param in &mut generics.params {
		if let GenericParam::Lifetime(lt) = param
			&& lt.lifetime == *original_lifetime
		{
			lt.lifetime = Lifetime::new(
				&format!("'{}", new_lifetime_str),
				proc_macro2::Span::call_site(),
			);
		}
	}

	let (_, ty_generics, _) = generics.split_for_impl();
	quote! { #name #ty_generics }
}

fn build_where_clause_with_clone<'a>(
	where_clause: Option<&syn::WhereClause>,
	field_types: impl Iterator<Item = &'a Type>,
) -> Option<syn::WhereClause> {
	let owned_field_types: Vec<_> = field_types.filter(|ty| !type_has_lifetime(ty)).collect();

	if owned_field_types.is_empty() {
		return where_clause.cloned();
	}

	let predicates: Vec<WherePredicate> = owned_field_types
		.iter()
		.map(|ty| syn::parse2(quote! { #ty: Clone }).unwrap())
		.collect();

	let mut extended_where_clause = where_clause.cloned();
	if let Some(where_clause) = &mut extended_where_clause {
		where_clause.predicates.extend(predicates);
	} else {
		extended_where_clause = Some(
			syn::parse2(quote! {
				where #(#predicates),*
			})
			.unwrap(),
		);
	}

	extended_where_clause
}

fn type_has_lifetime(ty: &Type) -> bool {
	// true
	match ty {
		Type::Reference(_) => true,
		Type::Path(type_path) => {
			// Check if any segment has lifetime arguments
			type_path.path.segments.iter().any(|segment| {
				match &segment.arguments {
					syn::PathArguments::None => false,
					syn::PathArguments::AngleBracketed(args) => {
						args.args.iter().any(|arg| match arg {
							syn::GenericArgument::Lifetime(_) => true,
							syn::GenericArgument::Type(ty) => type_has_lifetime(ty),
							_ => false,
						})
					}
					syn::PathArguments::Parenthesized(_) => false,
				}

				// true
			})
		}
		Type::Tuple(tuple) => tuple.elems.iter().any(type_has_lifetime),
		Type::Array(array) => type_has_lifetime(&array.elem),
		Type::Paren(paren) => type_has_lifetime(&paren.elem),
		_ => true,
	}
}

#[derive(Debug)]
struct CliDocFragment {
	id: String,
	content: String,
}
// (!) also needs to be updated in the generated code below

/// Searches source files for cli doc fragments,
/// which are doc comments used to generate CLI documentation.
/// See filen-cli/src/docs.rs
fn find_cli_doc_fragments() -> Vec<CliDocFragment> {
	// extract cli docs fragments from source files
	fn visit_source_files(dir: &std::path::Path) -> Vec<CliDocFragment> {
		let mut fragments = Vec::<CliDocFragment>::new();
		let cli_doc_comment_id_marker = regex::Regex::new(r"\/\/[\/!] \[cli-doc\] ?(.*)").unwrap();
		let cli_doc_comment_content = regex::Regex::new(r"\/\/[\/!] ?(.*)").unwrap();
		for entry in std::fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			if path.is_dir() {
				fragments.extend(visit_source_files(&path));
			} else if let Some(ext) = path.extension()
				&& ext == "rs"
			{
				// search files for markers and doc content
				let mut id_buffer: Option<String> = None;
				let mut content_buffer = String::new();
				fn flush_buffers(
					fragments: &mut Vec<CliDocFragment>,
					id_buffer: &mut Option<String>,
					content_buffer: &mut String,
				) {
					if let Some(id) = id_buffer.take() {
						fragments.push(CliDocFragment {
							id,
							content: content_buffer
								.trim_start_matches("\n")
								.trim_end_matches("\n")
								.to_string(),
						});
						content_buffer.clear();
					}
				}
				for line in std::fs::read_to_string(&path).unwrap().lines() {
					if let Some(doc_id) = cli_doc_comment_id_marker
						.captures(line.trim())
						.map(|captures| captures.get(1).unwrap().as_str())
					{
						// found doc id, flush buffers
						flush_buffers(&mut fragments, &mut id_buffer, &mut content_buffer);
						id_buffer = Some(doc_id.to_string());
					} else if let Some(doc_content) = cli_doc_comment_content
						.captures(line.trim())
						.map(|captures| captures.get(1).unwrap().as_str())
					{
						// found doc content, append if there is a current id
						if id_buffer.is_some() {
							content_buffer.push_str(doc_content);
							content_buffer.push('\n');
						}
					} else {
						// non-doc line, flush buffers
						flush_buffers(&mut fragments, &mut id_buffer, &mut content_buffer);
					}
				}
				flush_buffers(&mut fragments, &mut id_buffer, &mut content_buffer);
			}
		}
		fragments
	}
	let src_dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
	visit_source_files(&src_dir)
}

#[proc_macro]
pub fn extract_cli_doc_fragments(_item: TokenStream) -> TokenStream {
	let fragments = find_cli_doc_fragments();
	let serialized_fragments = fragments.iter().map(|frag| {
		let id = &frag.id;
		let content = &frag.content;
		quote! {
			CliDocFragment {
				id: #id.to_string(),
				content: #content.to_string(),
			}
		}
	});
	quote! {
		#[derive(Debug)]
		struct CliDocFragment {
			id: String,
			content: String,
		}

		fn get_cli_doc_fragments() -> Vec<CliDocFragment> {
			vec![
				#(#serialized_fragments),*
			]
		}
	}
	.into()
}

// fs traits

#[proc_macro_derive(HasParent)]
pub fn derive_has_parent(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::traits::HasParent),
		&[quote!(
			fn parent(&self) -> &filen_types::fs::ParentUuid;
		)],
	)
}

#[proc_macro_derive(HasRemoteInfo)]
pub fn derive_has_remote_info(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::traits::HasRemoteInfo),
		&[
			quote!(
				fn favorited(&self) -> bool;
			),
			quote!(
				fn timestamp(&self) -> chrono::DateTime<chrono::Utc>;
			),
		],
	)
}

#[proc_macro_derive(HasUUID)]
pub fn derive_has_uuid(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::traits::HasUUID),
		&[quote!(
			fn uuid(&self) -> &filen_types::fs::UuidStr;
		)],
	)
}

#[proc_macro_derive(HasName)]
pub fn derive_has_name(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::traits::HasName),
		&[quote!(
			fn name(&self) -> Option<&str>;
		)],
	)
}
#[proc_macro_derive(HasMeta)]
pub fn derive_has_meta(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::traits::HasMeta),
		&[quote!(
			fn get_meta_string(&self) -> Option<std::borrow::Cow<'_, str>>;
		)],
	)
}

// file traits

#[proc_macro_derive(HasRemoteFileInfo)]
pub fn derive_has_remote_file_info(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::file::traits::HasRemoteFileInfo),
		&[
			quote!(
				fn region(&self) -> &str;
			),
			quote!(
				fn bucket(&self) -> &str;
			),
			quote!(
				fn hash(&self) -> Option<filen_types::crypto::Blake3Hash>;
			),
		],
	)
}

#[proc_macro_derive(HasFileInfo)]
pub fn derive_has_file_info(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::file::traits::HasFileInfo),
		&[
			quote!(
				fn mime(&self) -> Option<&str>;
			),
			quote!(
				fn created(&self) -> Option<chrono::DateTime<chrono::Utc>>;
			),
			quote!(
				fn last_modified(&self) -> Option<chrono::DateTime<chrono::Utc>>;
			),
			quote!(
				fn size(&self) -> u64;
			),
			quote!(
				fn chunks(&self) -> u64;
			),
			quote!(
				fn key(&self) -> Option<&crate::crypto::file::FileKey>;
			),
		],
	)
}

#[proc_macro_derive(HasFileMeta)]
pub fn derive_has_file_meta(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::file::traits::HasFileMeta),
		&[quote!(
			fn get_meta(&self) -> &FileMeta<'_>;
		)],
	)
}

#[proc_macro_derive(File)]
pub fn derive_is_file(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(input, quote!(crate::fs::file::traits::File), &[])
}

// dir traits
#[proc_macro_derive(HasContents)]
pub fn derive_has_contents(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::dir::traits::HasContents),
		&[quote!(
			fn uuid_as_parent(&self) -> filen_types::fs::ParentUuid;
		)],
	)
}

#[proc_macro_derive(HasRemoteDirInfo)]
pub fn derive_has_remote_dir_info(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::dir::traits::HasRemoteDirInfo),
		&[quote!(
			fn color(&self) -> filen_types::api::v3::dir::color::DirColor<'_>;
		)],
	)
}

#[proc_macro_derive(HasDirInfo)]
pub fn derive_has_dir_info(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::dir::traits::HasDirInfo),
		&[quote!(
			fn created(&self) -> Option<chrono::DateTime<chrono::Utc>>;
		)],
	)
}

#[proc_macro_derive(HasDirMeta)]
pub fn derive_has_dir_meta(input: TokenStream) -> TokenStream {
	sdk_type_derives::delegate_trait(
		input,
		quote!(crate::fs::dir::traits::HasDirMeta),
		&[quote!(
			fn get_meta(&self) -> &crate::fs::dir::meta::DirectoryMeta<'_>;
		)],
	)
}

#[proc_macro_derive(CowFrom)]
pub fn cow_from_derive(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);
	let enum_name = &input.ident;

	// Extract the lifetime parameter (assume first lifetime is the one we need)
	let lifetime = input.generics.lifetimes().next().map(|l| &l.lifetime);

	let generics = &input.generics;

	let variants = match &input.data {
		Data::Enum(data_enum) => &data_enum.variants,
		_ => panic!("CowFrom can only be derived for enums"),
	};

	let mut impls = Vec::new();

	for variant in variants {
		let variant_name = &variant.ident;

		// Get the inner type from Cow<'a, T>
		if let Fields::Unnamed(fields) = &variant.fields {
			if fields.unnamed.len() != 1 {
				continue;
			}

			let field = fields.unnamed.first().unwrap();

			// Extract the inner type T from Cow<'a, T>
			if let Type::Path(TypePath { path, .. }) = &field.ty {
				let last_segment = path.segments.last().unwrap();

				if last_segment.ident != "Cow" {
					continue;
				}

				// Extract the generic argument (the T in Cow<'a, T>)
				if let syn::PathArguments::AngleBracketed(args) = &last_segment.arguments {
					// Find the type argument (skip lifetime arguments)
					let inner_type = args
						.args
						.iter()
						.find_map(|arg| {
							if let syn::GenericArgument::Type(ty) = arg {
								Some(ty)
							} else {
								None
							}
						})
						.expect("Cow should have a type argument");

					// Generate From<&'a T> for Enum<'a>
					if let Some(lt) = lifetime {
						let impl_borrowed = quote! {
							impl #generics From<&#lt #inner_type> for #enum_name #generics {
								fn from(value: &#lt #inner_type) -> Self {
									#enum_name::#variant_name(::std::borrow::Cow::Borrowed(value))
								}
							}
						};
						impls.push(impl_borrowed);
					}

					// Generate From<T> for Enum<'static>
					// Build generics with 'static lifetime
					let static_generics = if lifetime.is_some() {
						// Replace lifetime with 'static
						let params: Vec<_> = input
							.generics
							.params
							.iter()
							.map(|param| match param {
								GenericParam::Lifetime(_) => {
									let static_lt: Lifetime = syn::parse_quote!('static);
									quote! { #static_lt }
								}
								GenericParam::Type(ty) => {
									let ident = &ty.ident;
									quote! { #ident }
								}
								GenericParam::Const(c) => {
									let ident = &c.ident;
									quote! { #ident }
								}
							})
							.collect();

						if params.is_empty() {
							quote! {}
						} else {
							quote! { <#(#params),*> }
						}
					} else {
						quote! {}
					};

					let impl_owned = quote! {
						impl From<#inner_type> for #enum_name #static_generics {
							fn from(value: #inner_type) -> Self {
								#enum_name::#variant_name(::std::borrow::Cow::Owned(value))
							}
						}
					};
					impls.push(impl_owned);
				}
			}
		}
	}

	let expanded = quote! {
		#(#impls)*
	};

	TokenStream::from(expanded)
}
