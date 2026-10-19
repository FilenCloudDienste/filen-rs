use quote::ToTokens;

/// `tokens` without whitespace, so expansions compare token for token.
pub(crate) fn compact(tokens: impl ToTokens) -> String {
	tokens
		.into_token_stream()
		.to_string()
		.chars()
		.filter(|c| !c.is_whitespace())
		.collect()
}
