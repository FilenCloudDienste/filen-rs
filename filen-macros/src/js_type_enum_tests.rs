use super::*;
use crate::test_support::compact;

#[test]
fn fieldless_import_enum_uses_string_repr_not_untagged() {
	// e.g. the `LogLevel` enum: unit-only, must stay deserializable from JS.
	let item: ItemEnum = syn::parse_quote! {
		enum LogLevel {
			Off,
			Error,
			Warn,
			Info,
			Debug,
			Trace,
		}
	};
	assert!(is_fieldless_enum(&item));
	let repr = compact(default_enum_serde_repr(&item));
	assert_eq!(repr, r#"serde(rename_all="camelCase")"#);
	assert!(
		!repr.contains("untagged"),
		"fieldless enums must not be untagged: {repr}"
	);
}

#[test]
fn data_carrying_enum_keeps_untagged() {
	let item: ItemEnum = syn::parse_quote! {
		enum Value {
			Text(String),
			Number(u64),
		}
	};
	assert!(!is_fieldless_enum(&item));
	assert_eq!(compact(default_enum_serde_repr(&item)), "serde(untagged)");
}

fn expand(args: proc_macro2::TokenStream, item: ItemEnum) -> String {
	let args = syn::parse::Parser::parse2(
		syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
		args,
	)
	.unwrap();
	let state = parse_args(args.into_iter().collect()).unwrap();
	compact(enum_tokens(item, state))
}

#[test]
fn an_exported_unit_enum_is_written_as_its_variants_name_without_a_twin() {
	let item: ItemEnum = syn::parse_quote! {
		/// The phase.
		enum Phase {
			Running,
			Done,
		}
	};
	let tokens = expand(quote!(export, no_deser), item);
	assert!(
		tokens.contains(
			r#"derive(serde::Serialize),tsify(into_wasm_abi,large_number_types_as_bigints,hashmap_as_object),serde(rename_all="camelCase")"#
		),
		"{tokens}"
	);
	assert!(!tokens.contains("PhaseTagged"), "{tokens}");
	assert!(!tokens.contains("untagged"), "{tokens}");
	assert!(tokens.starts_with(r#"#[doc=r"Thephase."]"#), "{tokens}");
}

#[test]
fn a_tagged_enum_writes_its_fields_in_camel_case_only_when_asked() {
	let item: ItemEnum = syn::parse_quote! {
		enum Outcome {
			Kept { bytes_freed: u64 },
		}
	};
	let camel = expand(
		quote!(export, no_deser, tagged, camel_case_fields),
		item.clone(),
	);
	assert!(
		camel.contains(r#"serde(tag="type",rename_all="camelCase",rename_all_fields="camelCase")"#),
		"{camel}"
	);
	let plain = expand(quote!(export, no_deser, tagged), item);
	assert!(
		plain.contains(r#"serde(tag="type",rename_all="camelCase")"#),
		"{plain}"
	);
	assert!(!plain.contains("rename_all_fields"), "{plain}");
}

#[test]
fn enum_mixing_unit_and_data_variants_is_not_fieldless() {
	let item: ItemEnum = syn::parse_quote! {
		enum Mixed {
			Unit,
			Data(u8),
		}
	};
	assert!(!is_fieldless_enum(&item));
	assert_eq!(compact(default_enum_serde_repr(&item)), "serde(untagged)");
}
