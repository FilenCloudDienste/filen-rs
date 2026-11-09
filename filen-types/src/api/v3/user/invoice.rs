use serde::{Deserialize, Serialize};

use crate::{fs::Uuid, serde::str::Base64EncodedBytes};

pub const ENDPOINT: &str = "v3/user/invoice";

/// Generates the PDF of one invoice.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	/// The invoice's id, `UserAccountSubsInvoices::id`, not the subscription's.
	pub uuid: Uuid,
}

/// The PDF, which the backend sends as a bare base64 string in `data`.
pub type Response = Base64EncodedBytes<'static>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_invoice_is_read_from_a_bare_base64_string() {
		let response: Response = serde_json::from_str(r#""JVBERi0xLjQK""#).unwrap();
		assert_eq!(response.into_bytes(), b"%PDF-1.4\n");
	}

	#[test]
	fn an_invoice_that_is_not_base64_fails_to_parse() {
		assert!(serde_json::from_str::<Response>(r#""not base64!""#).is_err());
	}

	#[test]
	fn the_request_carries_the_invoice_uuid() {
		let uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
		assert_eq!(
			serde_json::to_string(&Request { uuid }).unwrap(),
			r#"{"uuid":"01234567-89ab-cdef-0123-456789abcdef"}"#
		);
	}
}
