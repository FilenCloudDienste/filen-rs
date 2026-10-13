//! Node-API (napi) output: mirrors for `#[js_type(napi)]` and `#[napi_mirror]` types, and
//! wrappers for the `#[napi_export]` methods of a `#[js_exports]` block.
//!
//! napi-derive generates the glue; this module writes every JS name and TypeScript type it prints,
//! rendered by [`ts::ubrn_ts`] with ubrn's rules, so the napi typings match what
//! uniffi-bindgen-react-native generates for the same items. napi's own type printing and its
//! cross-item name resolution are never relied on.
//!
//! Generated code names `::napi`, `::napi_derive` and the wire layer in `crate::js::napi`
//! (`Wire`, `FromWire`, `IntoWire`, `OptionReturn`, `throw`, `spawn_promise`), and sits behind
//! `#[cfg(feature = "napi")]`.
//!
//! Wire types are written syntactically around a leaf: an `Option<T>` is
//! `Option<<T as Wire>::Wire>`, a `Vec<T>` is `Vec<<T as Wire>::Wire>`, never
//! `<Option<T> as Wire>::Wire`, because napi-derive tells an optional object field by the literal
//! `Option` segment of its type.

#[cfg(test)]
mod tests;
mod ts;

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::visit_mut::VisitMut;
use syn::{
	Attribute, Field, Fields, FnArg, Ident, ImplItem, ImplItemFn, Item, ItemEnum, ItemImpl,
	ItemStruct, Meta, Pat, ReturnType, Type, Variant,
};

use ts::{Position, lower_camel, snake, ubrn_ts, upper_camel};

/// The directions a mirror converts in: `import` reads the type from JS, `export` writes it.
#[derive(Clone, Copy)]
pub(crate) struct Directions {
	pub(crate) import: bool,
	pub(crate) export: bool,
}

impl Directions {
	pub(crate) fn parse(args: Punctuated<Meta, Comma>) -> syn::Result<Self> {
		let mut directions = Self {
			import: false,
			export: false,
		};
		for arg in args {
			match arg {
				Meta::Path(path) if path.is_ident("import") => directions.import = true,
				Meta::Path(path) if path.is_ident("export") => directions.export = true,
				other => {
					return Err(syn::Error::new_spanned(
						other,
						"#[napi_mirror] takes `import`, `export` or both",
					));
				}
			}
		}
		Ok(directions)
	}

	/// napi-derive's flags turning off the direction this mirror does not convert in.
	fn napi_flags(self) -> TokenStream2 {
		match (self.import, self.export) {
			(true, false) => quote!(, object_to_js = false),
			(false, true) => quote!(, object_from_js = false),
			_ => quote!(),
		}
	}
}

/// The napi mirror of `item`, in a `#[cfg(feature = "napi")]` module next to it.
///
/// napi-derive copies no `#[cfg]` from a field or variant into the code it generates, and rustc
/// evaluates those `cfg`s only after it, so the mirror is written once per combination of the
/// `cfg` predicates on the item's fields and variants, each copy holding the members its
/// combination keeps, behind that combination.
pub(crate) fn mirror(item: &Item, directions: Directions) -> syn::Result<TokenStream2> {
	if !directions.import && !directions.export {
		return Err(syn::Error::new(
			Span::call_site(),
			"a napi mirror needs `import`, `export` or both",
		));
	}
	let (ident, generics) = match item {
		Item::Struct(item) => (&item.ident, &item.generics),
		Item::Enum(item) => (&item.ident, &item.generics),
		other => {
			return Err(syn::Error::new_spanned(
				other,
				"napi mirrors are generated for structs and enums only",
			));
		}
	};
	if !generics.params.is_empty() {
		return Err(syn::Error::new_spanned(
			generics,
			"napi mirrors of generic types have no rule yet",
		));
	}
	let module = format_ident!("__napi_{}", snake(&ident.unraw().to_string()));
	let predicates = member_cfgs(item)?;
	if predicates.len() > MAX_CFG_PREDICATES {
		return Err(syn::Error::new_spanned(
			ident,
			format!(
				"a napi mirror is written once per combination of its members' `cfg`s; more than \
				 {MAX_CFG_PREDICATES} distinct ones would be too many copies"
			),
		));
	}
	let mut copies = Vec::new();
	for kept in 0..1_u32 << predicates.len() {
		let conditions = predicates.iter().enumerate().map(|(i, predicate)| {
			if kept & 1 << i == 0 {
				quote!(not(#predicate))
			} else {
				quote!(#predicate)
			}
		});
		let body = match configure(item, &predicates, kept) {
			Item::Enum(item) => enum_mirror(&item, directions)?,
			Item::Struct(item) => struct_mirror(&item, directions)?,
			other => {
				return Err(syn::Error::new_spanned(
					other,
					"napi mirrors are generated for structs and enums only",
				));
			}
		};
		// The module holds nothing but napi items: napi-derive's generated FFI glue (the raw
		// N-API calls inside the conversion impls it writes for each item) is what needs the
		// allow.
		copies.push(quote! {
			#[cfg(all(feature = "napi" #(, #conditions)*))]
			#[allow(unsafe_code)]
			mod #module {
				use super::*;

				#body
			}
		});
	}
	Ok(quote!(#(#copies)*))
}

/// How many distinct `cfg` predicates a mirror's members may carry: each one doubles the copies.
const MAX_CFG_PREDICATES: usize = 3;

/// The predicate of each `#[cfg]` in `attrs`.
fn cfg_predicates(attrs: &[Attribute]) -> syn::Result<Vec<TokenStream2>> {
	attrs
		.iter()
		.filter(|attr| attr.path().is_ident("cfg"))
		.map(|attr| Ok(attr.meta.require_list()?.tokens.clone()))
		.collect()
}

/// The distinct `cfg` predicates on `item`'s fields and variants.
fn member_cfgs(item: &Item) -> syn::Result<Vec<TokenStream2>> {
	let mut attrs: Vec<&[Attribute]> = Vec::new();
	match item {
		Item::Struct(item) => attrs.extend(item.fields.iter().map(|field| &field.attrs[..])),
		Item::Enum(item) => {
			for variant in &item.variants {
				attrs.push(&variant.attrs);
				attrs.extend(variant.fields.iter().map(|field| &field.attrs[..]));
			}
		}
		_ => {}
	}
	let mut distinct: Vec<TokenStream2> = Vec::new();
	for attrs in attrs {
		for predicate in cfg_predicates(attrs)? {
			if !distinct
				.iter()
				.any(|seen| seen.to_string() == predicate.to_string())
			{
				distinct.push(predicate);
			}
		}
	}
	Ok(distinct)
}

/// Takes the `cfg`s off `attrs`, returning whether the combination `kept` (bit `i` set: the
/// predicate `predicates[i]` holds) keeps the member they were on.
fn configure_attrs(attrs: &mut Vec<Attribute>, predicates: &[TokenStream2], kept: u32) -> bool {
	let mut keep = true;
	attrs.retain(|attr| {
		if !attr.path().is_ident("cfg") {
			return true;
		}
		let Ok(list) = attr.meta.require_list() else {
			return true;
		};
		let predicate = list.tokens.to_string();
		let holds = predicates
			.iter()
			.position(|seen| seen.to_string() == predicate)
			.is_some_and(|i| kept & 1 << i != 0);
		keep &= holds;
		false
	});
	keep
}

fn configure_fields(fields: &mut Fields, predicates: &[TokenStream2], kept: u32) {
	let configured = |fields: &mut Punctuated<Field, Comma>| {
		*fields = std::mem::take(fields)
			.into_iter()
			.filter_map(|mut field| {
				configure_attrs(&mut field.attrs, predicates, kept).then_some(field)
			})
			.collect();
	};
	match fields {
		Fields::Named(named) => configured(&mut named.named),
		Fields::Unnamed(unnamed) => configured(&mut unnamed.unnamed),
		Fields::Unit => {}
	}
}

/// `item` as the combination `kept` of its members' `cfg` predicates configures it.
fn configure(item: &Item, predicates: &[TokenStream2], kept: u32) -> Item {
	let mut item = item.clone();
	match &mut item {
		Item::Struct(item) => configure_fields(&mut item.fields, predicates, kept),
		Item::Enum(item) => {
			item.variants = std::mem::take(&mut item.variants)
				.into_iter()
				.filter_map(|mut variant| {
					let keep = configure_attrs(&mut variant.attrs, predicates, kept);
					configure_fields(&mut variant.fields, predicates, kept);
					keep.then_some(variant)
				})
				.collect();
		}
		_ => {}
	}
	item
}

/// One field of a mirrored record or variant.
struct MirrorField<'a> {
	ident: &'a Ident,
	/// The field's type as written inside the mirror's module.
	ty: Type,
	/// The mirror's field definition.
	def: TokenStream2,
}

fn mirror_fields<'a>(
	fields: &'a Punctuated<Field, Comma>,
	item_ident: &Ident,
) -> syn::Result<Vec<MirrorField<'a>>> {
	fields
		.iter()
		.map(|field| {
			let Some(ident) = &field.ident else {
				return Err(syn::Error::new_spanned(
					field,
					"napi mirrors need named fields",
				));
			};
			let js_name = lower_camel(&ident.unraw().to_string());
			let ty = rebased(&field.ty, item_ident);
			let ts_type = ubrn_ts(&ty, Position::Field)?;
			let wire = wire_type(&ty);
			let docs = docs(&field.attrs);
			Ok(MirrorField {
				ident,
				ty,
				def: quote! {
					#(#docs)*
					#[napi(js_name = #js_name, ts_type = #ts_type)]
					pub #ident: #wire
				},
			})
		})
		.collect()
}

fn struct_mirror(item: &ItemStruct, directions: Directions) -> syn::Result<TokenStream2> {
	let Fields::Named(named) = &item.fields else {
		return Err(syn::Error::new_spanned(
			item,
			"napi mirrors need a struct with named fields",
		));
	};
	let name = &item.ident;
	let mirror = format_ident!("{}Napi", name.unraw());
	let js_name = upper_camel(&name.unraw().to_string());
	let flags = directions.napi_flags();
	let item_docs = docs(&item.attrs);
	let fields = mirror_fields(&named.named, name)?;
	let defs = fields.iter().map(|field| &field.def);

	let from_wire = directions.import.then(|| {
		let reads = fields.iter().map(|MirrorField { ident, ty, .. }| {
			quote! {
				#ident: <#ty as crate::js::napi::FromWire>::from_wire(wire.#ident)?
			}
		});
		quote! {
			impl crate::js::napi::FromWire for #name {
				fn from_wire(wire: #mirror) -> ::std::result::Result<Self, crate::Error> {
					::std::result::Result::Ok(Self { #(#reads,)* })
				}
			}
		}
	});
	let into_wire = directions.export.then(|| {
		let writes = fields.iter().map(|MirrorField { ident, ty, .. }| {
			quote! {
				#ident: <#ty as crate::js::napi::IntoWire>::into_wire(self.#ident)
			}
		});
		quote! {
			impl crate::js::napi::IntoWire for #name {
				fn into_wire(self) -> #mirror {
					#mirror { #(#writes,)* }
				}
			}
		}
	});

	Ok(quote! {
		#[::napi_derive::napi(object, js_name = #js_name #flags)]
		#(#item_docs)*
		pub struct #mirror {
			#(#defs,)*
		}

		impl crate::js::napi::Wire for #name {
			type Wire = #mirror;
		}

		#from_wire
		#into_wire
	})
}

fn enum_mirror(item: &ItemEnum, directions: Directions) -> syn::Result<TokenStream2> {
	if item.variants.is_empty() {
		return Err(syn::Error::new_spanned(
			item,
			"napi mirrors need at least one variant",
		));
	}
	if let Some((_, discriminant)) = item.variants.iter().find_map(|v| v.discriminant.as_ref()) {
		return Err(syn::Error::new_spanned(
			discriminant,
			"explicit discriminants have no napi rule yet",
		));
	}
	if item
		.variants
		.iter()
		.all(|variant| matches!(variant.fields, Fields::Unit))
	{
		Ok(flat_enum_mirror(item, directions))
	} else {
		data_enum_mirror(item, directions)
	}
}

/// The mirror's name for `variant`: ubrn's, which is also the tag napi writes for it.
fn variant_mirror_ident(variant: &Variant) -> Ident {
	Ident::new(
		&upper_camel(&variant.ident.unraw().to_string()),
		variant.ident.span(),
	)
}

/// An enum of unit variants: ubrn's numeric `enum`, numbered from 0 in declaration order.
fn flat_enum_mirror(item: &ItemEnum, directions: Directions) -> TokenStream2 {
	let name = &item.ident;
	let mirror = format_ident!("{}Napi", name.unraw());
	let js_name = upper_camel(&name.unraw().to_string());
	let item_docs = docs(&item.attrs);
	let variants = item.variants.iter().map(|variant| {
		let attrs = docs(&variant.attrs);
		let ident = variant_mirror_ident(variant);
		quote!(#(#attrs)* #ident)
	});
	let arms = item
		.variants
		.iter()
		.map(|variant| (&variant.ident, variant_mirror_ident(variant)));
	let from_wire = directions.import.then(|| {
		let arms = arms
			.clone()
			.map(|(ident, wire)| quote!(#mirror::#wire => Self::#ident));
		quote! {
			impl crate::js::napi::FromWire for #name {
				fn from_wire(wire: #mirror) -> ::std::result::Result<Self, crate::Error> {
					::std::result::Result::Ok(match wire { #(#arms,)* })
				}
			}
		}
	});
	let into_wire = directions.export.then(|| {
		let arms = arms
			.clone()
			.map(|(ident, wire)| quote!(Self::#ident => #mirror::#wire));
		quote! {
			impl crate::js::napi::IntoWire for #name {
				fn into_wire(self) -> #mirror {
					match self { #(#arms,)* }
				}
			}
		}
	});

	// The allow, here and on the data enums' mirrors: a mirror carries the variant names of the
	// item it mirrors, which are the API's, and clippy skips the lint only on the exported item.
	quote! {
		#[::napi_derive::napi(js_name = #js_name)]
		#(#item_docs)*
		#[allow(clippy::enum_variant_names)]
		pub enum #mirror {
			#(#variants,)*
		}

		impl crate::js::napi::Wire for #name {
			type Wire = #mirror;
		}

		#from_wire
		#into_wire
	}
}

/// An enum with data: ubrn's `{ tag, inner }` union and its `X_Tags` string enum.
///
/// napi cannot inline a variant's payload type (it refuses `skip_typescript` on an object), so
/// each struct variant's `inner` is an exported `X_V_Inner` object, which TypeScript compares
/// with ubrn's inline `Readonly<{ .. }>` structurally.
fn data_enum_mirror(item: &ItemEnum, directions: Directions) -> syn::Result<TokenStream2> {
	let name = &item.ident;
	let mirror = format_ident!("{}Napi", name.unraw());
	let tags = format_ident!("{}Tags", name.unraw());
	let js_name = upper_camel(&name.unraw().to_string());
	let tags_js_name = format!("{js_name}_Tags");
	let flags = directions.napi_flags();
	let item_docs = docs(&item.attrs);

	let mut payloads = Vec::new();
	let mut variants = Vec::new();
	let mut tag_variants = Vec::new();
	let mut reads = Vec::new();
	let mut writes = Vec::new();
	for variant in &item.variants {
		let ident = &variant.ident;
		let wire = variant_mirror_ident(variant);
		let attrs = docs(&variant.attrs);
		tag_variants.push(wire.clone());
		match &variant.fields {
			Fields::Unit => {
				variants.push(quote!(#(#attrs)* #wire));
				reads.push(quote!(#mirror::#wire => Self::#ident));
				writes.push(quote!(Self::#ident => #mirror::#wire));
			}
			Fields::Named(named) => {
				let payload = format_ident!("{}{}Inner", name.unraw(), wire);
				let payload_js_name = format!("{js_name}_{wire}_Inner");
				let fields = mirror_fields(&named.named, name)?;
				let defs = fields.iter().map(|field| &field.def);
				payloads.push(quote! {
					#[::napi_derive::napi(object, js_name = #payload_js_name #flags)]
					pub struct #payload {
						#(#defs,)*
					}
				});
				variants.push(quote! {
					#(#attrs)*
					#wire {
						#[napi(ts_type = #payload_js_name)]
						inner: #payload,
					}
				});
				let field_reads = fields.iter().map(|MirrorField { ident, ty, .. }| {
					quote! {
						#ident: <#ty as crate::js::napi::FromWire>::from_wire(inner.#ident)?
					}
				});
				reads.push(quote! {
					#mirror::#wire { inner } => Self::#ident { #(#field_reads,)* }
				});
				let bindings = fields
					.iter()
					.map(|MirrorField { ident, .. }| quote!(#ident));
				let field_writes = fields.iter().map(|MirrorField { ident, ty, .. }| {
					quote! {
						#ident: <#ty as crate::js::napi::IntoWire>::into_wire(#ident)
					}
				});
				writes.push(quote! {
					Self::#ident { #(#bindings,)* } => #mirror::#wire {
						inner: #payload { #(#field_writes,)* },
					}
				});
			}
			Fields::Unnamed(_) => {
				return Err(syn::Error::new_spanned(
					variant,
					"tuple variants have no napi rule yet (ubrn prints `inner: Readonly<[..]>`)",
				));
			}
		}
	}

	let from_wire = directions.import.then(|| {
		quote! {
			impl crate::js::napi::FromWire for #name {
				fn from_wire(wire: #mirror) -> ::std::result::Result<Self, crate::Error> {
					::std::result::Result::Ok(match wire { #(#reads,)* })
				}
			}
		}
	});
	let into_wire = directions.export.then(|| {
		quote! {
			impl crate::js::napi::IntoWire for #name {
				fn into_wire(self) -> #mirror {
					match self { #(#writes,)* }
				}
			}
		}
	});

	Ok(quote! {
		#(#payloads)*

		#[::napi_derive::napi(discriminant = "tag", js_name = #js_name #flags)]
		#(#item_docs)*
		#[allow(clippy::enum_variant_names)]
		pub enum #mirror {
			#(#variants,)*
		}

		#[::napi_derive::napi(string_enum, js_name = #tags_js_name)]
		#[allow(clippy::enum_variant_names)]
		pub enum #tags {
			#(#tag_variants,)*
		}

		impl crate::js::napi::Wire for #name {
			type Wire = #mirror;
		}

		#from_wire
		#into_wire
	})
}

/// `ty`'s wire type: `Option` and `Vec` written out around the leaf's `Wire` type.
fn wire_type(ty: &Type) -> TokenStream2 {
	if let Type::Path(path) = ty
		&& path.qself.is_none()
		&& let Some(last) = path.path.segments.last()
		&& let syn::PathArguments::AngleBracketed(args) = &last.arguments
		&& let [syn::GenericArgument::Type(inner)] = args.args.iter().collect::<Vec<_>>()[..]
	{
		let inner = wire_type(inner);
		if last.ident == "Option" {
			return quote!(::std::option::Option<#inner>);
		}
		if last.ident == "Vec" {
			return quote!(::std::vec::Vec<#inner>);
		}
	}
	quote!(<#ty as crate::js::napi::Wire>::Wire)
}

/// The docs on an item, which napi prints as JSDoc.
fn docs(attrs: &[Attribute]) -> Vec<&Attribute> {
	attrs
		.iter()
		.filter(|attr| attr.path().is_ident("doc"))
		.collect()
}

/// Takes the `#[napi_export]` markers off `item`'s methods and returns, for each marked method,
/// a module holding its napi wrapper as a `#[napi] impl` of the type's class in
/// `crate::js::napi`.
///
/// The class has the same name as the type, which is also its JS name: napi-derive names the
/// class a `#[napi] impl` registers by its JS name only when it has already expanded the class's
/// struct, and by the impl's type name otherwise, and expansion order is not ours to choose. As
/// that name would shadow the type, the wrapper's body sits in the module, in the scope of the
/// type, while the `#[napi] impl` sits in a module of its own that sees only the class, the body
/// and aliases of the wire types.
///
/// The module is behind `#[cfg(feature = "napi")]` and the method's own `cfg`s: napi-derive
/// copies no `cfg` from a method into the code it generates, so a `cfg` has to be on the whole
/// impl.
pub(crate) fn exports(item: &mut ItemImpl) -> syn::Result<TokenStream2> {
	let mut marked = Vec::new();
	for impl_item in &mut item.items {
		if let ImplItem::Fn(method) = impl_item {
			let before = method.attrs.len();
			method
				.attrs
				.retain(|attr| !attr.path().is_ident("napi_export"));
			if method.attrs.len() != before {
				marked.push(&*method);
			}
		}
	}
	if marked.is_empty() {
		return Ok(quote!());
	}
	let not_a_name = || {
		syn::Error::new_spanned(
			&item.self_ty,
			"#[napi_export] needs the block's type to be a plain name",
		)
	};
	let Type::Path(self_ty) = &*item.self_ty else {
		return Err(not_a_name());
	};
	let self_ident = self_ty.path.get_ident().ok_or_else(not_a_name)?;
	marked
		.into_iter()
		.map(|method| {
			let module = format_ident!(
				"__napi_export_{}_{}",
				snake(&self_ident.to_string()),
				method.sig.ident.unraw()
			);
			let predicates = cfg_predicates(&method.attrs)?;
			let Wrapper {
				aliases,
				call,
				method,
			} = export_wrapper(method, self_ident)?;
			let alias_idents = aliases.iter().map(|(alias, _)| alias);
			let alias_defs = aliases
				.iter()
				.map(|(alias, wire)| quote!(type #alias = #wire;));
			// Only `__napi_class` gets the allow: it holds nothing but the `#[napi] impl`, whose
			// generated FFI glue (the raw N-API calls of the method's callback) needs it, while the
			// body it forwards to is plain Rust under the workspace's `unsafe_code` deny.
			Ok(quote! {
				#[cfg(all(feature = "napi" #(, #predicates)*))]
				mod #module {
					use super::*;

					#(#alias_defs)*

					#call

					#[allow(unsafe_code)]
					mod __napi_class {
						use super::{__napi_call #(, #alias_idents)*};
						use crate::js::napi::#self_ident;

						#[::napi_derive::napi]
						impl #self_ident {
							#method
						}
					}
				}
			})
		})
		.collect()
}

/// Rewrites a type written next to the user's item so it means the same inside the generated
/// module one level below: `Self` becomes the item's name (in a wrapper, `Self` is the class),
/// a leading `super` gets one more `super`, and a leading `self` becomes `super`.
struct Rebase<'a>(&'a Ident);

impl VisitMut for Rebase<'_> {
	fn visit_type_path_mut(&mut self, path: &mut syn::TypePath) {
		if let Some(first) = path.path.segments.first_mut() {
			if first.ident == "Self" && path.qself.is_none() {
				first.ident = self.0.clone();
			} else if first.ident == "self" {
				first.ident = Ident::new("super", first.ident.span());
			} else if first.ident == "super" {
				let span = first.ident.span();
				path.path
					.segments
					.insert(0, Ident::new("super", span).into());
			}
		}
		syn::visit_mut::visit_type_path_mut(self, path);
	}
}

fn rebased(ty: &Type, self_ident: &Ident) -> Type {
	let mut ty = ty.clone();
	Rebase(self_ident).visit_type_mut(&mut ty);
	ty
}

/// The type a return is written as, its `Result` taken off: whether it was fallible, the type
/// whose wire type is returned, and whether a top-level `Option` became `OptionReturn` (napi
/// would return `None` as `null`; ubrn returns `undefined`).
struct Return {
	fallible: bool,
	ty: Type,
	optional: bool,
}

fn first_type_arg<'a>(ty: &'a Type, name: &str) -> Option<&'a Type> {
	let Type::Path(path) = ty else {
		return None;
	};
	let last = path.path.segments.last()?;
	if path.qself.is_some() || last.ident != name {
		return None;
	}
	let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
		return None;
	};
	args.args.iter().find_map(|arg| match arg {
		syn::GenericArgument::Type(ty) => Some(ty),
		_ => None,
	})
}

fn split_return(output: &ReturnType, self_ident: &Ident) -> Return {
	let ty = match output {
		ReturnType::Default => syn::parse_quote!(()),
		ReturnType::Type(_, ty) => rebased(ty, self_ident),
	};
	let (fallible, ty) = match first_type_arg(&ty, "Result") {
		Some(ok) => (true, ok.clone()),
		None => (false, ty),
	};
	match first_type_arg(&ty, "Option") {
		Some(some) => Return {
			fallible,
			ty: syn::parse_quote!(crate::js::napi::OptionReturn<#some>),
			optional: true,
		},
		None => Return {
			fallible,
			ty,
			optional: false,
		},
	}
}

/// The parts of one method's napi wrapper.
struct Wrapper {
	/// Aliases of the wire types the napi method takes and returns, written in the scope of the
	/// type, for the module of the `#[napi] impl` to name.
	aliases: Vec<(Ident, TokenStream2)>,
	/// `__napi_call`, the wrapper's body, in the scope of the type.
	call: TokenStream2,
	/// The napi method, which hands its args to `__napi_call`.
	method: TokenStream2,
}

/// The napi method calling `method`.
///
/// Params arrive as their wire types and are read with `FromWire`. A sync method throws a failed
/// read or its `Err` in ubrn's error shape through `crate::js::napi::throw`. An async method
/// returns a promise through `crate::js::napi::spawn_promise`, which takes ubrn's trailing
/// `asyncOpts_` and rejects with the error or an abort; its params are read with `FromWire` inside
/// that future, so a failed read (a bad uuid, a bigint out of range) rejects, as it does under
/// ubrn. napi-derive converts each JS argument to its wire type before the wrapper runs, though,
/// so a JS value of the wrong type (a `number` where a `bigint` belongs, a missing required key)
/// still throws synchronously from the method, where ubrn's `async` method would reject. A method
/// without a receiver is a static.
fn export_wrapper(method: &ImplItemFn, self_ident: &Ident) -> syn::Result<Wrapper> {
	let name = &method.sig.ident;
	let wrapper = format_ident!("__napi_{}", name.unraw());
	let js_name = lower_camel(&name.unraw().to_string());
	let method_docs = docs(&method.attrs);
	let is_async = method.sig.asyncness.is_some();

	let mut aliases = Vec::new();
	let mut params = Vec::new();
	let mut param_idents = Vec::new();
	let mut reads = Vec::new();
	let mut read_into = Vec::new();
	let mut args = Vec::new();
	let mut ts_args = Vec::new();
	let mut receiver = None;
	for input in &method.sig.inputs {
		let param = match input {
			FnArg::Receiver(found) => {
				receiver = Some(found);
				continue;
			}
			FnArg::Typed(param) => param,
		};
		let Pat::Ident(pat) = &*param.pat else {
			return Err(syn::Error::new_spanned(
				&param.pat,
				"#[napi_export] needs each param to be a plain name",
			));
		};
		let ident = &pat.ident;
		let ty = rebased(&param.ty, self_ident);
		ts_args.push(format!(
			"{}: {}",
			lower_camel(&ident.unraw().to_string()),
			ubrn_ts(&ty, Position::Param)?
		));
		// A borrowed param is read as its owned type and lent to the call.
		let (owned, lend): (Type, bool) = match &ty {
			Type::Reference(reference) => match &*reference.elem {
				Type::Path(path) if path.path.is_ident("str") => (syn::parse_quote!(String), true),
				elem => (elem.clone(), true),
			},
			_ => (ty.clone(), false),
		};
		let alias = format_ident!("__NapiParam{}", upper_camel(&ident.unraw().to_string()));
		aliases.push((alias.clone(), wire_type(&owned)));
		params.push(quote!(#ident: #alias));
		param_idents.push(ident);
		reads.push(quote!(<#owned as crate::js::napi::FromWire>::from_wire(#ident)));
		read_into.push(ident);
		args.push(if lend {
			quote!(&#ident)
		} else {
			quote!(#ident)
		});
	}

	let Return {
		fallible,
		ty: ret,
		optional,
	} = split_return(&method.sig.output, self_ident);
	let ret_ts = match &method.sig.output {
		ReturnType::Default => "void".to_owned(),
		ReturnType::Type(_, ty) => ubrn_ts(&rebased(ty, self_ident), Position::Return)?,
	};
	aliases.push((
		format_ident!("__NapiReturn"),
		quote!(<#ret as crate::js::napi::Wire>::Wire),
	));

	// The value the body calls the method on: the class's inner value, lent as `this`, cloned
	// when the call outlives the borrow (an async body) or consumes it.
	let (this, call_on) = match receiver.map(|receiver| &*receiver.ty) {
		None => (quote!(), quote!(#self_ident::)),
		Some(Type::Reference(_)) if !is_async => (quote!(), quote!(this.)),
		Some(Type::Reference(_)) => (
			quote!(let this = ::std::clone::Clone::clone(this);),
			quote!(this.),
		),
		Some(ty) if first_type_arg(ty, "Arc").is_some() => (
			quote!(let this = ::std::sync::Arc::new(::std::clone::Clone::clone(this));),
			quote!(this.),
		),
		Some(Type::Path(path)) if path.path.is_ident("Self") => (
			quote!(let this = ::std::clone::Clone::clone(this);),
			quote!(this.),
		),
		Some(ty) => {
			return Err(syn::Error::new_spanned(
				ty,
				"#[napi_export] takes `&self`, `self` or `self: Arc<Self>`",
			));
		}
	};
	let self_param = receiver.map(|_| quote!(&self,));
	let this_param = receiver.map(|_| quote!(this: &#self_ident,));
	let this_arg = receiver.map(|_| quote!(&self.0,));
	let call = if is_async {
		quote!(#call_on #name(#(#args),*).await)
	} else {
		quote!(#call_on #name(#(#args),*))
	};

	let wrap = |value: TokenStream2| {
		if optional {
			quote!(crate::js::napi::OptionReturn(#value))
		} else {
			value
		}
	};

	if is_async {
		ts_args.push("asyncOpts_?: { signal: AbortSignal }".to_owned());
		let ts_args = ts_args.join(", ");
		let ts_ret = format!("Promise<{ret_ts}>");
		let output = if fallible && optional {
			quote!(::std::result::Result::map(#call, crate::js::napi::OptionReturn))
		} else if fallible {
			call
		} else {
			let value = wrap(call);
			quote!(::std::result::Result::<_, crate::Error>::Ok(#value))
		};
		let returns = quote! {
			::napi::Result<::napi::bindgen_prelude::PromiseRaw<'env, __NapiReturn>>
		};
		Ok(Wrapper {
			aliases,
			call: quote! {
				fn __napi_call<'env>(
					#this_param
					env: &'env ::napi::Env,
					#(#params,)*
					async_opts: ::std::option::Option<::napi::bindgen_prelude::Object<'env>>,
				) -> #returns {
					#this
					crate::js::napi::spawn_promise(env, async_opts, async move {
						#(let #read_into = #reads?;)*
						#output
					})
				}
			},
			method: quote! {
				#(#method_docs)*
				#[napi(js_name = #js_name, ts_args_type = #ts_args, ts_return_type = #ts_ret)]
				pub fn #wrapper<'env>(
					#self_param
					env: &'env ::napi::Env,
					#(#params,)*
					async_opts: ::std::option::Option<::napi::bindgen_prelude::Object<'env>>,
				) -> #returns {
					__napi_call(#this_arg env, #(#param_idents,)* async_opts)
				}
			},
		})
	} else {
		let ts_args = ts_args.join(", ");
		let value = if fallible {
			wrap(quote!(#call.map_err(|e| crate::js::napi::throw(env, e))?))
		} else {
			wrap(call)
		};
		let needs_env = fallible || !params.is_empty();
		let env_param = needs_env.then(|| quote!(env: &::napi::Env,));
		let env_arg = needs_env.then(|| quote!(env,));
		Ok(Wrapper {
			aliases,
			call: quote! {
				fn __napi_call(
					#this_param
					#env_param
					#(#params,)*
				) -> ::napi::Result<__NapiReturn> {
					#(let #read_into = #reads.map_err(|e| crate::js::napi::throw(env, e))?;)*
					#this
					::std::result::Result::Ok(<#ret as crate::js::napi::IntoWire>::into_wire(#value))
				}
			},
			method: quote! {
				#(#method_docs)*
				#[napi(js_name = #js_name, ts_args_type = #ts_args, ts_return_type = #ret_ts)]
				pub fn #wrapper(
					#self_param
					#env_param
					#(#params,)*
				) -> ::napi::Result<__NapiReturn> {
					__napi_call(#this_arg #env_arg #(#param_idents,)*)
				}
			},
		})
	}
}
