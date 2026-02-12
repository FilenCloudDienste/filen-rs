use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, TraitItemFn, parse_macro_input};

pub(crate) fn delegate_trait(
	input: TokenStream,
	trait_name: proc_macro2::TokenStream,
	methods: &[proc_macro2::TokenStream],
) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);

	let enum_name = &input.ident;
	let generics = &input.generics;
	let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

	let Data::Enum(data_enum) = &input.data else {
		panic!("this delegate macro only work on enums");
	};

	let variants: Vec<_> = data_enum.variants.iter().map(|v| &v.ident).collect();

	let method_impls = methods.iter().map(|method_sig| {
		// Parse the method signature
		let method: TraitItemFn =
			syn::parse2(method_sig.clone()).expect("Invalid method signature");
		let method_name = &method.sig.ident;
		let inputs = &method.sig.inputs; // This is a Punctuated list
		let output = &method.sig.output;

		// Extract parameter names for forwarding (skip &self)
		let param_names: Vec<_> = inputs
			.iter()
			.filter_map(|arg| {
				if let syn::FnArg::Typed(pat_type) = arg
					&& let syn::Pat::Ident(ident) = &*pat_type.pat
				{
					return Some(&ident.ident);
				}
				None
			})
			.collect();

		// Generate match arms
		let match_arms = variants.iter().map(|variant| {
			quote! {
				Self::#variant(inner) => #trait_name::#method_name(inner.as_ref() #(, #param_names)*)
			}
		});

		quote! {
			fn #method_name(#inputs) #output {
				match self {
					#(#match_arms),*
				}
			}
		}
	});

	let expanded = quote! {
		impl #impl_generics #trait_name for #enum_name #ty_generics #where_clause {
			#(#method_impls)*
		}
	};

	TokenStream::from(expanded)
}
