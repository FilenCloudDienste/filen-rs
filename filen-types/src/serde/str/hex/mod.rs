use generic_array::{ArrayLength, GenericArray};

pub(super) mod heap_unsized;
pub(super) mod stack_sized;

pub fn hex_decode_to_generic_array<T: AsRef<[u8]>, N: ArrayLength>(
	input: T,
) -> Result<GenericArray<u8, N>, hex::FromHexError> {
	let mut output = GenericArray::<u8, N>::uninit();

	// SAFETY, copying implementation of decode_to_array using GenericArray instead of [u8; N]
	let output_slice = unsafe { output.assume_init_mut() };

	hex::decode_to_slice(input, output_slice).map(|()| {
		// SAFETY, copying implementation of decode_to_array using GenericArray instead of [u8; N]
		unsafe { GenericArray::<u8, N>::assume_init(output) }
	})
}
