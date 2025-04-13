pub(crate) mod number {
	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<bool, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		struct BooleanVisitor;
		impl serde::de::Visitor<'_> for BooleanVisitor {
			type Value = bool;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a boolean or number")
			}

			fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
				Ok(v)
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
				Ok(v != 0)
			}

			fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
				Ok(v != 0)
			}

			fn visit_u8<E>(self, v: u8) -> Result<Self::Value, E> {
				Ok(v != 0)
			}
		}
		deserializer.deserialize_any(BooleanVisitor)
	}

	pub(crate) fn serialize<S>(v: &bool, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let value = if *v { 1 } else { 0 };
		serializer.serialize_u8(value)
	}
}
