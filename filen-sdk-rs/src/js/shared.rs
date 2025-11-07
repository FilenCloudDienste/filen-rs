pub(super) enum EncodedOrDecoded<E, D> {
	Encoded(E),
	Decoded(D),
}

pub(super) trait AsEncodedOrDecoded<'a, E, D, E1, D1>
where
	E: 'a,
	D: 'a,
	E1: 'static,
	D1: 'static,
{
	fn as_encoded_or_decoded(&'a self) -> EncodedOrDecoded<E, D>;
	fn from_encoded(encoded: E1) -> Self;
	fn from_decoded(decoded: D1) -> Self;
	fn from_encoded_or_decoded(encoded: Option<E1>, decoded: Option<D1>) -> Option<Self>
	where
		Self: Sized,
	{
		match (encoded, decoded) {
			// prefer decoded if both are present
			(_, Some(decoded)) => Some(Self::from_decoded(decoded)),
			(Some(encoded), None) => Some(Self::from_encoded(encoded)),
			(None, None) => None,
		}
	}
}
