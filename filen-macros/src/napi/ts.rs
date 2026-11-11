//! The TypeScript uniffi-bindgen-react-native (ubrn) prints for a Rust type, and uniffi's casing
//! of identifiers, so the napi typings can be written to match it.

use syn::{GenericArgument, PathArguments, PathSegment, Type, TypePath};

/// Where a type appears, which decides what it may be: `()` and `Result` only in a return, an
/// `Option` nowhere inside another type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Position {
	Field,
	Param,
	Return,
	/// An `Option`'s or a `Vec`'s type argument.
	Element,
}

/// The TypeScript ubrn prints for `ty` at `pos`.
///
/// A type with no rule yet is an error naming the rule to add, never a guess: a guess would
/// type-check in TypeScript and fail at run time.
pub(crate) fn ubrn_ts(ty: &Type, pos: Position) -> syn::Result<String> {
	match ty {
		Type::Reference(reference) => ubrn_ts(&reference.elem, pos),
		Type::Paren(paren) => ubrn_ts(&paren.elem, pos),
		Type::Group(group) => ubrn_ts(&group.elem, pos),
		Type::Tuple(tuple) if tuple.elems.is_empty() => match pos {
			Position::Return => Ok("void".to_owned()),
			Position::Field | Position::Param | Position::Element => {
				Err(no_rule(ty, "`()` is only a return type"))
			}
		},
		Type::Tuple(_) => Err(no_rule(ty, "tuples have no napi rule yet")),
		Type::Path(path) if path.qself.is_none() => path_ts(ty, path, pos),
		_ => Err(no_rule(ty, "this type has no napi rule yet")),
	}
}

fn path_ts(ty: &Type, path: &TypePath, pos: Position) -> syn::Result<String> {
	let Some(last) = path.path.segments.last() else {
		return Err(no_rule(ty, "an empty path has no napi rule"));
	};
	let Some(args) = type_args(last) else {
		return Err(no_rule(ty, "this type has no napi rule yet"));
	};
	let name = last.ident.to_string();
	Ok(match (name.as_str(), args.as_slice()) {
		("String" | "str" | "Uuid" | "UuidStr", []) => "string".to_owned(),
		("bool", []) => "boolean".to_owned(),
		("u8" | "u16" | "u32" | "i8" | "i16" | "i32" | "f64", []) => "number".to_owned(),
		("u64" | "i64", []) => "bigint".to_owned(),
		("f32", []) => {
			return Err(no_rule(
				ty,
				"`f32` has no napi rule yet: napi cannot read one, so wire it as f64 and narrow it \
				 with ToPrimitive",
			));
		}
		("usize" | "isize" | "u128" | "i128", []) => {
			return Err(no_rule(
				ty,
				"uniffi has no such integer: widen it in the type that crosses the boundary",
			));
		}
		("char", []) => return Err(no_rule(ty, "uniffi has no `char`: use a String")),
		("Self", []) => return Err(no_rule(ty, "`Self` must be replaced by the type's name")),
		// uniffi lowers a chrono `DateTime` as its milliseconds, an `i64` (filen-types'
		// `uniffi_impls`).
		("DateTime", _) => "bigint".to_owned(),
		("SystemTime" | "Duration" | "Instant", _) => {
			return Err(no_rule(
				ty,
				"`SystemTime`, `Duration` and `Instant` have no napi rule yet (ubrn prints a Date)",
			));
		}
		// ubrn types an object as its class's interface wherever another type holds it.
		("Error" | "FilenSdkError", []) => "FilenSdkErrorInterface".to_owned(),
		("HashMap" | "BTreeMap" | "HashSet" | "BTreeSet", _) => {
			return Err(no_rule(
				ty,
				"maps and sets have no napi rule yet (ubrn prints a Map)",
			));
		}
		// napi leaves a `None` field's key out and the wrappers return `undefined` for a `None`
		// return, but writes `null` for a `None` anywhere else, where ubrn's `undefined` would lie.
		("Option", _) if pos == Position::Element => {
			return Err(no_rule(
				ty,
				"an `Option` inside another type has no napi rule yet: napi writes its `None` as \
				 null, ubrn types it as undefined",
			));
		}
		("Option", [inner]) => format!("{} | undefined", ubrn_ts(inner, Position::Element)?),
		// ubrn prints an ArrayBuffer, which napi cannot own: the bytes cross as a Uint8Array,
		// copied by `crate::js::napi::bytes` where they are read and written.
		("Vec", [inner]) if is_u8(inner) => match pos {
			Position::Field | Position::Param => "Uint8Array".to_owned(),
			Position::Return | Position::Element => {
				return Err(no_rule(
					ty,
					"a `Vec<u8>` has a napi rule only as a field or a param (a Uint8Array)",
				));
			}
		},
		("Vec", [inner]) => format!("Array<{}>", ubrn_ts(inner, Position::Element)?),
		("Result", [ok, ..]) if pos == Position::Return => ubrn_ts(ok, Position::Return)?,
		("Result", _) => return Err(no_rule(ty, "`Result` is only a return type")),
		("Arc", [inner]) => ubrn_ts(inner, pos)?,
		// uniffi crosses it as its custom type of that name (filen-types' `crypto`).
		("MaybeEncrypted", [inner]) if is_str(inner) => "MaybeEncryptedStatic".to_owned(),
		// `#[js_type]`'s `XTagged` twin is wasm's; on uniffi and napi it is an alias of `X`,
		// which is the type ubrn prints.
		(_, []) => upper_camel(
			name.strip_suffix("Tagged")
				.filter(|aliased| !aliased.is_empty())
				.unwrap_or(&name),
		),
		_ => return Err(no_rule(ty, "this generic type has no napi rule yet")),
	})
}

/// The type arguments of `segment`, lifetimes left out; `None` for `Fn(..)`-style arguments.
fn type_args(segment: &PathSegment) -> Option<Vec<&Type>> {
	match &segment.arguments {
		PathArguments::None => Some(Vec::new()),
		PathArguments::AngleBracketed(args) => Some(
			args.args
				.iter()
				.filter_map(|arg| match arg {
					GenericArgument::Type(ty) => Some(ty),
					_ => None,
				})
				.collect(),
		),
		PathArguments::Parenthesized(_) => None,
	}
}

fn is_u8(ty: &Type) -> bool {
	matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident("u8"))
}

fn is_str(ty: &Type) -> bool {
	matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident("str"))
}

fn no_rule(ty: &Type, why: &str) -> syn::Error {
	syn::Error::new_spanned(ty, format!("napi: {why} (filen-macros `ubrn_ts`)"))
}

/// The words of an identifier, split the way heck splits them (uniffi names everything through
/// heck): at every non-alphanumeric char, between a lowercase char and an uppercase one, and
/// before the last of a run of uppercase chars that a lowercase one follows (`HTTPServer` is
/// `HTTP`, `Server`). Digits never split.
fn words(s: &str) -> Vec<String> {
	#[derive(Clone, Copy, PartialEq, Eq)]
	enum Mode {
		Boundary,
		Lowercase,
		Uppercase,
	}

	let mut words = Vec::new();
	for part in s.split(|c: char| !c.is_alphanumeric()) {
		let mut word = String::new();
		let mut mode = Mode::Boundary;
		let mut chars = part.chars().peekable();
		while let Some(c) = chars.next() {
			let Some(&next) = chars.peek() else {
				word.push(c);
				break;
			};
			let next_mode = if c.is_lowercase() {
				Mode::Lowercase
			} else if c.is_uppercase() {
				Mode::Uppercase
			} else {
				mode
			};
			if next_mode == Mode::Lowercase && next.is_uppercase() {
				word.push(c);
				words.push(std::mem::take(&mut word));
				mode = Mode::Boundary;
			} else if mode == Mode::Uppercase && c.is_uppercase() && next.is_lowercase() {
				words.push(std::mem::take(&mut word));
				word.push(c);
				mode = Mode::Boundary;
			} else {
				word.push(c);
				mode = next_mode;
			}
		}
		if !word.is_empty() {
			words.push(word);
		}
	}
	words
}

fn capitalized(word: &str) -> String {
	let mut chars = word.chars();
	chars
		.next()
		.into_iter()
		.flat_map(char::to_uppercase)
		.chain(chars.flat_map(char::to_lowercase))
		.collect()
}

/// heck's `to_lower_camel_case`, uniffi's name for fields, methods and params.
pub(crate) fn lower_camel(s: &str) -> String {
	words(s)
		.iter()
		.enumerate()
		.map(|(i, word)| {
			if i == 0 {
				word.to_lowercase()
			} else {
				capitalized(word)
			}
		})
		.collect()
}

/// heck's `to_upper_camel_case`, uniffi's name for types and enum variants.
pub(crate) fn upper_camel(s: &str) -> String {
	words(s).iter().map(|word| capitalized(word)).collect()
}

/// heck's `to_snake_case`, for the names of generated modules.
pub(crate) fn snake(s: &str) -> String {
	words(s)
		.iter()
		.map(|word| word.to_lowercase())
		.collect::<Vec<_>>()
		.join("_")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ts(ty: Type, pos: Position) -> String {
		ubrn_ts(&ty, pos).unwrap()
	}

	fn error(ty: Type, pos: Position) -> String {
		ubrn_ts(&ty, pos).unwrap_err().to_string()
	}

	#[test]
	fn strings_and_ids_are_strings() {
		assert_eq!(ts(syn::parse_quote!(String), Position::Field), "string");
		assert_eq!(ts(syn::parse_quote!(&str), Position::Param), "string");
		assert_eq!(ts(syn::parse_quote!(Uuid), Position::Field), "string");
		assert_eq!(
			ts(syn::parse_quote!(filen_types::fs::UuidStr), Position::Field),
			"string"
		);
	}

	#[test]
	fn a_bool_is_a_boolean() {
		assert_eq!(ts(syn::parse_quote!(bool), Position::Param), "boolean");
	}

	#[test]
	fn integers_up_to_32_bits_and_f64_are_numbers() {
		for ty in [
			quote::quote!(u8),
			quote::quote!(u16),
			quote::quote!(u32),
			quote::quote!(i8),
			quote::quote!(i16),
			quote::quote!(i32),
			quote::quote!(f64),
		] {
			assert_eq!(ts(syn::parse2(ty).unwrap(), Position::Field), "number");
		}
	}

	#[test]
	fn u64_and_i64_are_bigints() {
		assert_eq!(ts(syn::parse_quote!(u64), Position::Field), "bigint");
		assert_eq!(ts(syn::parse_quote!(i64), Position::Param), "bigint");
	}

	#[test]
	fn an_option_is_its_type_or_undefined_in_every_position() {
		for pos in [Position::Field, Position::Param, Position::Return] {
			assert_eq!(
				ts(syn::parse_quote!(Option<u32>), pos),
				"number | undefined"
			);
		}
	}

	#[test]
	fn a_vec_is_an_array() {
		assert_eq!(
			ts(syn::parse_quote!(Vec<Vec<String>>), Position::Field),
			"Array<Array<string>>"
		);
	}

	#[test]
	fn an_option_inside_a_vec_or_an_option_is_an_error() {
		for pos in [Position::Field, Position::Param, Position::Return] {
			assert!(
				error(syn::parse_quote!(Vec<Option<u32>>), pos)
					.contains("napi writes its `None` as null")
			);
			assert!(
				error(syn::parse_quote!(Option<Option<u32>>), pos)
					.contains("napi writes its `None` as null")
			);
		}
	}

	#[test]
	fn a_result_is_its_ok_type_in_a_return() {
		assert_eq!(
			ts(syn::parse_quote!(Result<JsClient, Error>), Position::Return),
			"JsClient"
		);
		assert_eq!(
			ts(syn::parse_quote!(crate::Result<()>), Position::Return),
			"void"
		);
		assert_eq!(
			ts(
				syn::parse_quote!(Result<Option<u64>, Error>),
				Position::Return
			),
			"bigint | undefined"
		);
	}

	#[test]
	fn a_result_or_unit_outside_a_return_is_an_error() {
		assert!(
			error(syn::parse_quote!(Result<u8, Error>), Position::Field)
				.contains("`Result` is only a return type")
		);
		assert!(
			error(syn::parse_quote!(()), Position::Param).contains("`()` is only a return type")
		);
		assert_eq!(ts(syn::parse_quote!(()), Position::Return), "void");
	}

	#[test]
	fn an_arc_or_a_reference_is_its_target() {
		assert_eq!(
			ts(
				syn::parse_quote!(std::sync::Arc<JsClient>),
				Position::Return
			),
			"JsClient"
		);
		assert_eq!(
			ts(syn::parse_quote!(&LoginParams), Position::Param),
			"LoginParams"
		);
	}

	#[test]
	fn any_other_type_is_its_last_ident_in_upper_camel_case() {
		assert_eq!(
			ts(syn::parse_quote!(crate::js::LoginParams), Position::Param),
			"LoginParams"
		);
		assert_eq!(
			ts(syn::parse_quote!(TwoFASecret), Position::Return),
			"TwoFaSecret"
		);
	}

	#[test]
	fn a_tagged_twin_is_the_type_it_aliases() {
		assert_eq!(
			ts(syn::parse_quote!(NonRootDirTagged), Position::Field),
			"NonRootDir"
		);
		assert_eq!(
			ts(syn::parse_quote!(Vec<NonRootItemTagged>), Position::Field),
			"Array<NonRootItem>"
		);
	}

	#[test]
	fn a_maybe_encrypted_string_is_uniffis_custom_type() {
		assert_eq!(
			ts(
				syn::parse_quote!(MaybeEncrypted<'static, str>),
				Position::Field
			),
			"MaybeEncryptedStatic"
		);
		assert!(
			error(syn::parse_quote!(MaybeEncrypted<'a, [u8]>), Position::Field)
				.contains("generic type has no napi rule")
		);
	}

	#[test]
	fn f32_is_an_error() {
		assert!(error(syn::parse_quote!(f32), Position::Field).contains("`f32` has no napi rule"));
	}

	#[test]
	fn integers_uniffi_lacks_are_errors() {
		for ty in [
			quote::quote!(usize),
			quote::quote!(isize),
			quote::quote!(u128),
			quote::quote!(i128),
		] {
			assert!(
				error(syn::parse2(ty).unwrap(), Position::Field)
					.contains("uniffi has no such integer")
			);
		}
	}

	#[test]
	fn a_date_time_is_a_bigint() {
		assert_eq!(
			ts(syn::parse_quote!(chrono::DateTime<Utc>), Position::Field),
			"bigint"
		);
		assert_eq!(
			ts(syn::parse_quote!(Option<DateTime<Utc>>), Position::Param),
			"bigint | undefined"
		);
	}

	#[test]
	fn a_byte_vector_is_a_uint8_array_as_a_field_or_a_param_only() {
		assert_eq!(
			ts(syn::parse_quote!(Vec<u8>), Position::Field),
			"Uint8Array"
		);
		assert_eq!(
			ts(syn::parse_quote!(Vec<u8>), Position::Param),
			"Uint8Array"
		);
		for (ty, pos) in [
			(syn::parse_quote!(Vec<u8>), Position::Return),
			(syn::parse_quote!(Option<Vec<u8>>), Position::Field),
			(syn::parse_quote!(Vec<Vec<u8>>), Position::Param),
		] {
			assert!(
				error(ty, pos).contains("only as a field or a param"),
				"{pos:?}"
			);
		}
	}

	#[test]
	fn the_sdk_error_is_its_interface() {
		assert_eq!(
			ts(syn::parse_quote!(Arc<crate::Error>), Position::Field),
			"FilenSdkErrorInterface"
		);
		assert_eq!(
			ts(syn::parse_quote!(FilenSdkError), Position::Param),
			"FilenSdkErrorInterface"
		);
	}

	#[test]
	fn maps_tuples_and_std_times_are_errors() {
		assert!(
			error(syn::parse_quote!(HashMap<String, u32>), Position::Field)
				.contains("maps and sets")
		);
		for ty in [
			quote::quote!(std::time::SystemTime),
			quote::quote!(Duration),
			quote::quote!(Instant),
		] {
			assert!(
				error(syn::parse2(ty).unwrap(), Position::Field)
					.contains("`SystemTime`, `Duration` and `Instant` have no napi rule")
			);
		}
		assert!(error(syn::parse_quote!((u32, u32)), Position::Field).contains("tuples"));
	}

	#[test]
	fn an_unknown_generic_type_is_an_error() {
		assert!(
			error(syn::parse_quote!(Cow<'a, str>), Position::Field)
				.contains("generic type has no napi rule")
		);
		assert!(error(syn::parse_quote!(Self), Position::Return).contains("`Self`"));
	}

	#[test]
	fn names_are_cased_as_heck_cases_them() {
		assert_eq!(lower_camel("generate_2fa_secret"), "generate2faSecret");
		assert_eq!(
			lower_camel("upload_bandwidth_kilobytes_per_sec"),
			"uploadBandwidthKilobytesPerSec"
		);
		assert_eq!(lower_camel("from_config"), "fromConfig");
		assert_eq!(upper_camel("TwoFASecret"), "TwoFaSecret");
		assert_eq!(upper_camel("IO"), "Io");
		assert_eq!(upper_camel("Enter2fa"), "Enter2fa");
		assert_eq!(upper_camel("HTTPServer"), "HttpServer");
		assert_eq!(upper_camel("JsClientConfig"), "JsClientConfig");
		assert_eq!(snake("CacheStatusMessage"), "cache_status_message");
		assert_eq!(snake("TwoFASecret"), "two_fa_secret");
	}
}
