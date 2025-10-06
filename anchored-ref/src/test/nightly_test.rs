use super::*;

#[test]
fn test_closure_lifetime_binder() {
	let data = Bytes::from(r#"{"k": "\n", "k1": "asdf"}"#);

	println!("Data: {:?}", data);
	let combined: AnchoredRef<Bytes, MyStruct<'static>> = create_combined_with_serde(data.clone());

	println!("Combined: {:?}", combined);

	let combined =
		combined.map::<_, AnotherStruct>(for<'a> |parts: MyStruct<'a>| -> AnotherStruct<'a> {
			AnotherStruct {
				name: parts.k,
				value: parts.k1,
			}
		});

	combined.with_ref(|deserialized| {
		println!("Deserialized: {:?}", deserialized);

		assert_eq!("\n", deserialized.name);
		assert!(matches!(deserialized.name, Cow::Owned(_)));
		assert_eq!("asdf", deserialized.value.as_ref());
		assert!(matches!(deserialized.value, Cow::Borrowed(_)));

		// This line would now cause a compile error:
		// deserialized // Error: lifetime may not live long enough
	});
}
