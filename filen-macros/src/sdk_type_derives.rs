use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, TraitItemFn, parse_macro_input};

pub(crate) fn delegate_trait(
	input: TokenStream,
	trait_name: proc_macro2::TokenStream,
	methods: &[proc_macro2::TokenStream],
) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);

	let type_name = &input.ident;
	let generics = &input.generics;
	let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

	// Determine how to generate the method body based on the data type
	enum DelegationKind {
		Enum(Vec<proc_macro2::Ident>),
		Newtype,
	}

	let delegation_kind = match &input.data {
		Data::Enum(data_enum) => {
			let variants: Vec<_> = data_enum.variants.iter().map(|v| v.ident.clone()).collect();
			DelegationKind::Enum(variants)
		}
		Data::Struct(data_struct) => {
			// Must be a newtype: exactly one unnamed field
			match &data_struct.fields {
				syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
					DelegationKind::Newtype
				}
				_ => panic!(
					"delegate_trait on a struct only supports newtype wrappers (exactly one unnamed field)"
				),
			}
		}
		Data::Union(_) => panic!("delegate_trait does not support unions"),
	};

	let method_impls = methods.iter().map(|method_sig| {
		let method: TraitItemFn =
			syn::parse2(method_sig.clone()).expect("Invalid method signature");
		let method_name = &method.sig.ident;
		let inputs = &method.sig.inputs;
		let output = &method.sig.output;

		let param_names: Vec<_> = inputs
			.iter()
			.filter_map(|arg| {
				if let syn::FnArg::Typed(pat_type) = arg
					&& let syn::Pat::Ident(ident) = &*pat_type.pat
				{
					return Some(ident.ident.clone());
				}
				None
			})
			.collect();

		let body = match &delegation_kind {
			DelegationKind::Enum(variants) => {
				let match_arms = variants.iter().map(|variant| {
					quote! {
						Self::#variant(inner) => #trait_name::#method_name(inner.as_ref() #(, #param_names)*)
					}
				});
				quote! {
					match self {
						#(#match_arms),*
					}
				}
			}
			DelegationKind::Newtype => {
				quote! {
					#trait_name::#method_name(&self.0 #(, #param_names)*)
				}
			}
		};

		quote! {
			fn #method_name(#inputs) #output {
				#body
			}
		}
	});

	let expanded = quote! {
		impl #impl_generics #trait_name for #type_name #ty_generics #where_clause {
			#(#method_impls)*
		}
	};

	TokenStream::from(expanded)
}
