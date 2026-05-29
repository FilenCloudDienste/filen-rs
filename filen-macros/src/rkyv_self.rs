use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Type};

/// Wires up the rkyv "archived form is the type itself" traits for a struct or
/// enum.
///
/// Everything is produced by rkyv's and bytecheck's own derive macros; this
/// macro only injects those derives, the `#[rkyv(as = Self, ...)]` bounds, and a
/// suitable `repr`:
/// - `#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]` with
///   `#[rkyv(as = Self)]` makes `Archived = Self`,
/// - `#[derive(rkyv::Portable)]` validates the `repr` and bounds each field
///   `: Portable`,
/// - `#[derive(rkyv::bytecheck::CheckBytes)]` (unless `no_check_bytes`) validates
///   each field — and, for enums, the discriminant.
///
/// See the [`rkyv_self`](macro@crate::rkyv_self) attribute for the contract.
pub(crate) fn rkyv_self(item: TokenStream, no_check_bytes: bool) -> TokenStream {
	let mut input = parse_macro_input!(item as DeriveInput);

	// For the whole type to archive `as = Self`, every field type must archive
	// *as itself*; collect the distinct field types to build those bounds.
	let field_types = match collect_field_types(&input.data) {
		Ok(types) => types,
		Err(err) => return err.to_compile_error().into(),
	};

	if let Err(err) = ensure_repr(&mut input) {
		return err.to_compile_error().into();
	}

	// `as = Self` itself refuses to *also* generate a `CheckBytes` impl, but a
	// standalone bytecheck `CheckBytes` derive is independent and composes with it.
	let check_bytes = if no_check_bytes {
		quote!()
	} else {
		quote!(, ::rkyv::bytecheck::CheckBytes)
	};
	input.attrs.push(parse_quote!(#[derive(
		::rkyv::Archive,
		::rkyv::Serialize,
		::rkyv::Deserialize,
		::rkyv::Portable
		#check_bytes
	)]));
	if !no_check_bytes {
		// bytecheck's derive emits `::bytecheck::..` paths by default; redirect it
		// to the copy re-exported by rkyv, since this crate depends on `rkyv`
		// (which re-exports `bytecheck`) rather than on `bytecheck` directly.
		input
			.attrs
			.push(parse_quote!(#[bytecheck(crate = ::rkyv::bytecheck)]));
	}

	// `archive_bounds`/`deserialize_bounds` make each field archive as itself,
	// which is what the generated resolve/deserialize code requires (and what
	// makes the impls hold for generic field types).
	if field_types.is_empty() {
		input.attrs.push(parse_quote!(#[rkyv(as = Self)]));
	} else {
		let bounds = field_types
			.iter()
			.map(|ty| quote!(#ty: ::rkyv::Archive<Archived = #ty>))
			.collect::<Vec<_>>();
		input.attrs.push(parse_quote!(#[rkyv(
			as = Self,
			archive_bounds(#(#bounds),*),
			deserialize_bounds(#(#bounds),*),
		)]));
	}

	quote!(#input).into()
}

/// Collects the distinct field types of a struct, or of every enum variant,
/// preserving first-seen order. Unions are rejected.
fn collect_field_types(data: &Data) -> syn::Result<Vec<Type>> {
	let field_types: Vec<&Type> = match data {
		Data::Struct(data) => data.fields.iter().map(|field| &field.ty).collect(),
		Data::Enum(data) => data
			.variants
			.iter()
			.flat_map(|variant| variant.fields.iter().map(|field| &field.ty))
			.collect(),
		Data::Union(data) => {
			return Err(syn::Error::new_spanned(
				data.union_token,
				"#[rkyv_self] cannot be applied to unions",
			));
		}
	};

	let mut distinct: Vec<Type> = Vec::new();
	let mut seen: Vec<String> = Vec::new();
	for ty in field_types {
		let key = quote!(#ty).to_string();
		if !seen.contains(&key) {
			seen.push(key);
			distinct.push(ty.clone());
		}
	}
	Ok(distinct)
}

/// Ensures the item carries a `repr` that yields a stable, target-independent
/// layout (which `Portable` requires).
///
/// An explicit `repr` is left untouched — the `Portable`/`CheckBytes` derives
/// validate it and emit clear errors for unsuitable reprs. Otherwise a
/// single-field struct gets `#[repr(transparent)]` (the tightest guarantee), any
/// other struct gets `#[repr(C)]`, and an enum is rejected because its
/// discriminant size cannot be inferred.
fn ensure_repr(input: &mut DeriveInput) -> syn::Result<()> {
	if input.attrs.iter().any(|attr| attr.path().is_ident("repr")) {
		return Ok(());
	}

	match &input.data {
		Data::Struct(data) => {
			if data.fields.len() == 1 {
				input.attrs.push(parse_quote!(#[repr(transparent)]));
			} else {
				input.attrs.push(parse_quote!(#[repr(C)]));
			}
			Ok(())
		}
		Data::Enum(_) => Err(syn::Error::new_spanned(
			&input.ident,
			"#[rkyv_self] on an enum requires an explicit primitive `repr`, e.g. `#[repr(u8)]`",
		)),
		// Unions are rejected by `collect_field_types`, which runs first.
		Data::Union(_) => Ok(()),
	}
}
