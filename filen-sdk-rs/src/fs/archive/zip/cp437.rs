//! Code page 437, the character set zip names are in unless an entry says it is UTF-8, or was
//! made on Unix, whose tools store UTF-8 without saying so.

/// The characters of bytes 0x80 to 0xFF; bytes below are ASCII.
const HIGH: [char; 128] = [
	'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
	'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
	'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
	'╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
	'╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
	'▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
	'≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{A0}',
];

pub(crate) fn decode(bytes: &[u8]) -> String {
	bytes
		.iter()
		.map(|&byte| match byte {
			0..=0x7F => char::from(byte),
			_ => HIGH[usize::from(byte - 0x80)],
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cp437_decodes_ascii_as_is_and_the_high_half_by_table() {
		assert_eq!(decode(b"plain.txt"), "plain.txt");
		assert_eq!(decode(&[0x80, 0x81, 0xE1, 0xFF]), "Çüß\u{A0}");
	}
}
