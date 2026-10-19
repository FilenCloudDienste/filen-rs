use super::*;
use crate::test_support::compact;

#[test]
fn arc_self_method_keeps_ffi_name_and_calls_through_self() {
	let mut method: syn::ImplItemFn = syn::parse_quote! {
		async fn get_thumbnail(self: std::sync::Arc<Self>, size: u32) -> Vec<u8> {
			Vec::new()
		}
	};
	let wrapper = get_wrapper_fn(&mut method).unwrap();
	let tokens = compact(&wrapper);

	// The wrapper must be renamed back to the original FFI name...
	assert!(
		tokens.contains(r#"uniffi::method(name="get_thumbnail")"#),
		"missing method rename attribute: {tokens}"
	);
	// ...defined under the mangled internal name...
	assert!(tokens.contains("uniffi_get_thumbnail"), "tokens: {tokens}");
	// ...and its body must call the original method through `self.`, passing
	// only the real argument. A double-self bug would emit
	// `self.get_thumbnail(self,size)`, of which `self.get_thumbnail(size)` is
	// not a substring; the old code emitted `Self::get_thumbnail(self,size)`.
	assert!(
		tokens.contains("self.get_thumbnail(size)"),
		"body should call self.get_thumbnail(size): {tokens}"
	);
	assert!(
		!tokens.contains("Self::get_thumbnail"),
		"Arc<Self> receiver should not be called as a static fn: {tokens}"
	);
}

#[test]
fn ref_self_method_still_renamed() {
	let mut method: syn::ImplItemFn = syn::parse_quote! {
		async fn refresh(&self, force: bool) -> bool {
			force
		}
	};
	let wrapper = get_wrapper_fn(&mut method).unwrap();
	let tokens = compact(&wrapper);

	assert!(
		tokens.contains(r#"uniffi::method(name="refresh")"#),
		"missing method rename attribute: {tokens}"
	);
	assert!(
		tokens.contains("self.refresh(force)"),
		"body should call self.refresh(force): {tokens}"
	);
}
