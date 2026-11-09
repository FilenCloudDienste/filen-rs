use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/sub/create";

/// How a new subscription is paid for.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PaymentMethod {
	/// A card, through Stripe.
	Stripe,
	/// PayPal.
	Paypal,
	/// Cryptocurrency, through Coinbase. The backend accepts it for lifetime plans only.
	Crypto,
}

/// Opens a checkout for a plan.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	/// The plan's id, a JSON number on the wire.
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub plan_id: u64,
	/// How the plan is paid for.
	pub method: PaymentMethod,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	/// Where the user pays: the payment provider's checkout page.
	pub url: String,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_request_sends_plan_id_as_a_number_and_method_under_its_key_in_lowercase() {
		for (method, wire) in [
			(PaymentMethod::Stripe, "stripe"),
			(PaymentMethod::Paypal, "paypal"),
			(PaymentMethod::Crypto, "crypto"),
		] {
			assert_eq!(
				serde_json::to_string(&Request {
					plan_id: 42,
					method
				})
				.unwrap(),
				format!(r#"{{"planId":42,"method":"{wire}"}}"#)
			);
		}
	}

	#[test]
	fn the_response_carries_the_checkout_url() {
		let response: Response =
			serde_json::from_str(r#"{"url":"https://checkout.stripe.com/c/pay/cs_test"}"#).unwrap();
		assert_eq!(response.url, "https://checkout.stripe.com/c/pay/cs_test");
	}
}
