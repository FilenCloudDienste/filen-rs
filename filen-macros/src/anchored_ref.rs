use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, GenericParam, Lifetime, WhereClause, parse_macro_input};

/// Derive macro for TransmuteLifetime trait
///
/// Generates different implementations based on whether the struct has lifetime parameters:
/// - For types with lifetimes: implements for 'static version with transmute
/// - For types without lifetimes: implements identity operations
pub(crate) fn derive_transmute_lifetime(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);

	let name = &input.ident;
	let generics = &input.generics;
	let where_clause = &generics.where_clause;

	// Extract lifetime and type parameters
	let (lifetime_params, type_params, const_params): (Vec<_>, Vec<_>, Vec<_>) = generics
		.params
		.iter()
		.fold((Vec::new(), Vec::new(), Vec::new()), |mut acc, param| {
			match param {
				GenericParam::Lifetime(lt) => acc.0.push(&lt.lifetime),
				GenericParam::Type(tp) => acc.1.push(&tp.ident),
				GenericParam::Const(cp) => acc.2.push(&cp.ident),
			}
			acc
		});

	if lifetime_params.is_empty() {
		// Case 1: No lifetime parameters (like String)
		generate_no_lifetime_impl(name, &type_params, &const_params, where_clause)
	} else {
		// Case 2: Has lifetime parameters (like MyStruct<'a>)
		generate_lifetime_impl(
			name,
			&lifetime_params,
			&type_params,
			&const_params,
			where_clause,
		)
	}
}

/// Generate implementation for types without lifetime parameters
fn generate_no_lifetime_impl(
	name: &syn::Ident,
	type_params: &[&syn::Ident],
	const_params: &[&syn::Ident],
	where_clause: &Option<WhereClause>,
) -> TokenStream {
	let generics = if type_params.is_empty() && const_params.is_empty() {
		quote! {}
	} else {
		quote! { <#(#type_params),* #(, #const_params)*> }
	};

	let expanded = quote! {
		unsafe impl #generics anchored_ref::TransmuteLifetime for #name #generics #where_clause {
			type Borrowed<'transmute_lifetime> = #name #generics;

			unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
				borrowed
			}

			fn transmute_from_static<'transmute_lifetime>(static_val: Self) -> Self::Borrowed<'transmute_lifetime> {
				static_val
			}
		}
	};

	expanded.into()
}

/// Generate implementation for types with lifetime parameters
fn generate_lifetime_impl(
	name: &syn::Ident,
	lifetime_params: &[&Lifetime],
	type_params: &[&syn::Ident],
	const_params: &[&syn::Ident],
	where_clause: &Option<WhereClause>,
) -> TokenStream {
	// Create 'static versions of all lifetime parameters for the impl target
	let static_lifetimes = lifetime_params.iter().map(|_| quote! { 'static });

	// For the Borrowed type, use 'a for all lifetime positions
	let borrowed_lifetimes = lifetime_params
		.iter()
		.map(|_| quote! { 'transmute_lifetime });

	// Build the generic arguments for the static version (impl target)
	let static_generics =
		if lifetime_params.is_empty() && type_params.is_empty() && const_params.is_empty() {
			quote! {}
		} else {
			quote! { <#(#static_lifetimes),* #(, #type_params),* #(, #const_params)*> }
		};

	// Build the generic arguments for the borrowed version
	let borrowed_generics =
		if lifetime_params.is_empty() && type_params.is_empty() && const_params.is_empty() {
			quote! {}
		} else {
			quote! { <#(#borrowed_lifetimes),* #(, #type_params),* #(, #const_params)*> }
		};

	// Build impl generics (type and const params only, since lifetimes are concrete)
	let impl_generics = if type_params.is_empty() && const_params.is_empty() {
		quote! {}
	} else {
		quote! { <#(#type_params),* #(, #const_params)*> }
	};

	let expanded = quote! {
		unsafe impl #impl_generics anchored_ref::TransmuteLifetime for #name #static_generics #where_clause {
			type Borrowed<'transmute_lifetime> = #name #borrowed_generics;

			unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
				unsafe { std::mem::transmute(borrowed) }
			}

			fn transmute_from_static<'transmute_lifetime>(static_val: Self) -> Self::Borrowed<'transmute_lifetime> {
				static_val
			}
		}
	};

	expanded.into()
}
