use std::borrow::Cow;

use bytes::Bytes;

use super::*;

pub fn create_combined_with_serde<T>(data: Bytes) -> AnchoredRef<Bytes, T>
where
	T: TransmuteLifetime + 'static,
	for<'de> T::Borrowed<'de>: serde::Deserialize<'de>,
{
	AnchoredRef::new(data, |bytes| serde_json::from_slice(bytes).unwrap())
}

#[derive(serde::Deserialize, Debug)]
struct MyStruct<'a> {
	#[serde(borrow)]
	k: Cow<'a, str>,
	#[serde(borrow)]
	k1: Cow<'a, str>,
}

unsafe impl crate::TransmuteLifetime for MyStruct<'static> {
	type Borrowed<'a> = MyStruct<'a>;

	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
		unsafe { std::mem::transmute(borrowed) }
	}

	fn transmute_from_static<'a>(static_val: Self) -> Self::Borrowed<'a> {
		static_val
	}
}

unsafe impl crate::TransmuteLifetime for String {
	type Borrowed<'a> = String;

	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
		borrowed
	}

	fn transmute_from_static<'a>(static_val: Self) -> Self::Borrowed<'a> {
		static_val
	}
}

#[derive(serde::Deserialize, Debug)]
struct AnotherStruct<'a> {
	#[serde(borrow)]
	name: Cow<'a, str>,
	#[serde(borrow)]
	value: Cow<'a, str>,
}

// For types that don't borrow, we can implement this trivially
unsafe impl crate::TransmuteLifetime for i32 {
	type Borrowed<'a> = i32;

	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
		borrowed
	}

	fn transmute_from_static<'a>(static_val: Self) -> Self::Borrowed<'a> {
		static_val
	}
}

unsafe impl crate::TransmuteLifetime for AnotherStruct<'static> {
	type Borrowed<'a> = AnotherStruct<'a>;

	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
		unsafe { std::mem::transmute(borrowed) }
	}

	fn transmute_from_static<'a>(static_val: Self) -> Self::Borrowed<'a> {
		static_val
	}
}

// todo work on this, right now there's no way to contain a generic type that may borrow

// struct GenericStruct<'a, T>
// where
// 	T: TransmuteLifetime<Borrowed<'static> = T> + 'static,
// {
// 	data: T::Borrowed<'a>,
// }

// unsafe impl<T> crate::TransmuteLifetime for GenericStruct<'static, T>
// where
// 	T: TransmuteLifetime<Borrowed<'static> = T> + 'static,
// 	// T: T::Borrowed<'static>,
// {
// 	type Borrowed<'a> = GenericStruct<'a, T>;

// 	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self {
// 		GenericStruct {
// 			data: unsafe { T::transmute_to_static(borrowed.data) },
// 		}
// 	}

// 	fn transmute_from_static<'a>(static_val: Self) -> Self::Borrowed<'a> {
// 		GenericStruct {
// 			data: T::transmute_from_static(static_val.data),
// 		}
// 	}
// }

#[test]
fn test_with_mystruct() {
	let data = Bytes::from(r#"{"k": "\n", "k1": "asdf"}"#);

	println!("Data: {:?}", data);
	let combined: AnchoredRef<Bytes, MyStruct<'static>> = create_combined_with_serde(data);

	println!("Combined: {:?}", combined);

	combined.with_ref(|deserialized| {
		println!("Deserialized: {:?}", deserialized);

		assert_eq!("\n", deserialized.k);
		assert!(matches!(deserialized.k, Cow::Owned(_)));
		assert_eq!("asdf", deserialized.k1.as_ref());
		assert!(matches!(deserialized.k1, Cow::Borrowed(_)));

		// This line would now cause a compile error:
		// deserialized // Error: lifetime may not live long enough
	});
}

#[test]
fn test_with_mystruct1() {
	let data = Bytes::from(r#"{"k": "\n", "k1": "asdf"}"#);

	println!("Data: {:?}", data);
	let combined: AnchoredRef<Bytes, MyStruct<'static>> = create_combined_with_serde(data);

	println!("Combined: {:?}", combined);

	let combined = combined.map::<_, AnotherStruct>(|parts, _| AnotherStruct {
		name: parts.k,
		value: parts.k1,
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

#[test]
fn test_with_anotherstruct() {
	let data = Bytes::from(r#"{"name": "test", "value": "data"}"#);

	println!("Data: {:?}", data);
	let combined: AnchoredRef<Bytes, AnotherStruct<'static>> = create_combined_with_serde(data);

	println!("Combined: {:?}", combined);

	combined.with_ref(|deserialized| {
		println!("Deserialized: {:?}", deserialized);

		assert_eq!("test", deserialized.name);
		assert_eq!("data", deserialized.value.as_ref());
	});
}

#[test]
fn test_with_non_borrowing_type() {
	let data = Bytes::from(r#""hello world""#);

	let combined: AnchoredRef<Bytes, String> = create_combined_with_serde(data);

	combined.with_ref(|deserialized| {
		assert_eq!("hello world", deserialized);
	});
}

#[test]
fn test_with_custom_deserializer() {
	let data = Bytes::from("42");

	let combined: AnchoredRef<Bytes, i32> = AnchoredRef::new(data, |bytes| {
		std::str::from_utf8(bytes).unwrap().parse().unwrap()
	});

	combined.with_ref(|deserialized| {
		assert_eq!(42, deserialized);
	});
}
