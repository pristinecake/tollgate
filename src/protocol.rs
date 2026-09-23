//! The x402 v2 wire format, as Rust types.
//!
//! Kite ships a TypeScript SDK (`@x402/core`) and a Python one (`x402`). There
//! is no Rust SDK, and this module is what stands in for it. The protocol turns
//! out to be small enough to implement directly — three JSON documents and
//! three headers — and doing so removes a whole dependency tree from the build,
//! which for a small edge service is the difference between a 4 MB binary and a
//! 40 MB one.
//!
//! The three documents:
//!
//! - **challenge** (`PaymentRequired`) — what the server sends with a 402,
//!   base64'd into `PAYMENT-REQUIRED`.
//! - **payment** (`PaymentPayload`) — what the client sends back, base64'd into
//!   `PAYMENT-SIGNATURE`.
//! - **receipt** (`SettleResponse`) — what the facilitator returns after it
//!   lands the transfer, echoed back base64'd in `PAYMENT-RESPONSE`.
//!
//! `SAY` is the authority on the exact field names. Where upstream spells a
//! field in camelCase, the `#[serde(rename)]` below is not cosmetic: dropping
//! one makes every paid request come back 402 with no explanation.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::ProtocolError;

/// The only protocol version this server speaks.
pub const X402_VERSION: u8 = 2;

/// Carries the base64 [`PaymentRequired`] challenge on a 402 response.
pub const HEADER_PAYMENT_REQUIRED: &str = "PAYMENT-REQUIRED";
/// Carries the base64 [`PaymentPayload`] from the client.
pub const HEADER_PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
/// Carries the base64 [`SettleResponse`] on a settled response.
pub const HEADER_PAYMENT_RESPONSE: &str = "PAYMENT-RESPONSE";
/// Version 1 spelling of the payment header. Still read, so older agents work.
pub const HEADER_X_PAYMENT: &str = "X-PAYMENT";

/// The `exact` scheme's identifier.
pub const SCHEME_EXACT: &str = "exact";

/// Where a resource lives and what it is. Rides along in the challenge so the
/// client knows what it is about to pay for before it signs anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// One acceptable way to pay. A challenge carries a list of these; this server
/// emits exactly one, because a route settles on exactly one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    pub scheme: String,
    pub network: String,
    /// ERC-20 contract address of the settling stablecoin.
    pub asset: String,
    /// Price in the token's smallest unit, as a decimal string.
    pub amount: String,
    /// Address that receives the transfer.
    pub pay_to: String,
    /// How long a signed authorization stays valid, in seconds.
    pub max_timeout_seconds: u64,
    /// Scheme-specific extras. For `exact` on an EIP-3009 token this carries the
    /// EIP-712 domain (`{"name": ..., "version": ...}`).
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The 402 body. Version 2 puts this in a header rather than the response body,
/// so a browser hitting a paid endpoint still gets readable JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    /// Why the previous payment, if any, was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub resource: ResourceInfo,
    pub accepts: Vec<PaymentRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
}

/// An EIP-3009 `transferWithAuthorization` payload: the signed authorization
/// plus the signature over it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferAuthorization {
    pub from: String,
    pub to: String,
    /// Amount in the token's smallest unit.
    pub value: String,
    /// Unix seconds before which the authorization is not yet valid.
    pub valid_after: String,
    /// Unix seconds after which the authorization is dead.
    pub valid_before: String,
    /// 32-byte replay nonce, hex encoded.
    pub nonce: String,
}

/// The `payload` object inside a [`PaymentPayload`] for the `exact` scheme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactEvmPayload {
    pub authorization: TransferAuthorization,
    /// 65-byte secp256k1 signature, hex encoded.
    pub signature: String,
    /// `transferWithAuthorization` for EIP-3009 tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_type: Option<String>,
}

/// What the client sends back after reading a challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceInfo>,
    /// The challenge entry the client chose to satisfy. The facilitator checks
    /// the signature against *this*, so it has to be echoed back faithfully.
    pub accepted: PaymentRequirements,
    pub payload: ExactEvmPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Body of `POST /verify` and `POST /settle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub x402_version: u8,
    pub payment_payload: PaymentPayload,
    pub payment_requirements: PaymentRequirements,
}

/// Facilitator's answer to `/verify`. A `false` here means "do not run the
/// handler, do not settle" — the caller is told why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    pub is_valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
}

/// Facilitator's answer to `/settle`. `transaction` is required by the schema
/// even on failure, where it is an empty string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    pub transaction: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
}

/// One scheme/network pair the facilitator is willing to service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedKind {
    pub x402_version: u8,
    pub scheme: String,
    pub network: String,
}

/// Facilitator's answer to `GET /supported`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportedResponse {
    pub kinds: Vec<SupportedKind>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub signers: std::collections::BTreeMap<String, Vec<String>>,
}

impl TransferAuthorization {
    /// The address that will be debited.
    pub fn payer(&self) -> &str {
        &self.from
    }
}

impl PaymentPayload {
    /// Reject a payment that does not actually pay for the route it was
    /// presented to.
    ///
    /// The facilitator verifies the signature, but it verifies it against
    /// whatever `accepted` says — and `accepted` is attacker-controlled input.
    /// A client can pay 1 atomic unit to its own address on a route that costs
    /// a dollar and every signature check still passes. So the server has to
    /// compare the payment against the price it actually quoted.
    ///
    /// Addresses are compared case-insensitively because checksums vary by
    /// producer.
    ///
    /// # Errors
    ///
    /// A [`ProtocolError`] variant per mismatch, so the 402 can say which field
    /// was wrong.
    pub fn check_against(&self, expected: &PaymentRequirements) -> Result<(), ProtocolError> {
        if self.x402_version != X402_VERSION {
            return Err(ProtocolError::VersionMismatch {
                found: self.x402_version,
                expected: X402_VERSION,
            });
        }

        if !eq_hex(&self.accepted.asset, &expected.asset) {
            return Err(ProtocolError::AssetMismatch);
        }

        if !eq_hex(&self.accepted.pay_to, &expected.pay_to)
            || !eq_hex(&self.payload.authorization.to, &expected.pay_to)
        {
            return Err(ProtocolError::PayToMismatch {
                found: self.accepted.pay_to.clone(),
                expected: expected.pay_to.clone(),
            });
        }

        // Both the `accepted` echo and the signed value have to cover the price.
        for claimed in [&self.accepted.amount, &self.payload.authorization.value] {
            if compare_atomic(claimed, &expected.amount) == Ordering::Less {
                return Err(ProtocolError::AmountTooLow {
                    found: claimed.clone(),
                    expected: expected.amount.clone(),
                });
            }
        }

        Ok(())
    }
}

/// Compare two decimal-atomic-unit strings as integers, without parsing them
/// into a fixed-width type that `10^18` would overflow.
///
/// Longer is bigger once leading zeros are gone; equal lengths compare
/// lexicographically, which for digit strings is numeric order.
pub fn compare_atomic(left: &str, right: &str) -> Ordering {
    let left = left.trim().trim_start_matches('0');
    let right = right.trim().trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn eq_hex(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

/// base64 a JSON document for transport in a header.
///
/// # Errors
///
/// Returns the serde error if the value cannot be serialized (which for these
/// types means a bug, not bad input).
pub fn encode_header<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    use base64::Engine as _;
    let json = serde_json::to_vec(value)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json))
}

/// Undo [`encode_header`].
///
/// # Errors
///
/// [`ProtocolError::BadBase64`] when the header is not base64, and
/// [`ProtocolError::BadJson`] when it decodes but is not the expected shape.
pub fn decode_header<T: DeserializeOwned>(
    header: &'static str,
    raw: &str,
) -> Result<T, ProtocolError> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .map_err(|source| ProtocolError::BadBase64 { header, source })?;
    serde_json::from_slice(&bytes).map_err(|source| ProtocolError::BadJson { header, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirements() -> PaymentRequirements {
        PaymentRequirements {
            scheme: SCHEME_EXACT.to_owned(),
            network: "eip155:2366".to_owned(),
            asset: "0x7aB6f3ed87C42eF0aDb67Ed95090f8bF5240149e".to_owned(),
            amount: "1000".to_owned(),
            pay_to: "0x1234567890123456789012345678901234567890".to_owned(),
            max_timeout_seconds: 60,
            extra: serde_json::Map::new(),
        }
    }

    fn payload_for(accepted: PaymentRequirements, value: &str) -> PaymentPayload {
        PaymentPayload {
            x402_version: X402_VERSION,
            resource: None,
            accepted,
            payload: ExactEvmPayload {
                authorization: TransferAuthorization {
                    from: "0xAbCdEfAbCdEfAbCdEfAbCdEfAbCdEfAbCdEfAbCd".to_owned(),
                    to: "0x1234567890123456789012345678901234567890".to_owned(),
                    value: value.to_owned(),
                    valid_after: "0".to_owned(),
                    valid_before: "9999999999".to_owned(),
                    nonce: format!("0x{}", "00".repeat(32)),
                },
                signature: format!("0x{}", "ab".repeat(65)),
                authorization_type: Some("transferWithAuthorization".to_owned()),
            },
            extensions: None,
        }
    }

    #[test]
    fn challenge_survives_a_header_round_trip() {
        let challenge = PaymentRequired {
            x402_version: X402_VERSION,
            error: None,
            resource: ResourceInfo {
                url: "https://api.example.com/v1/forecast".to_owned(),
                description: Some("one forecast".to_owned()),
                mime_type: Some("application/json".to_owned()),
            },
            accepts: vec![requirements()],
            extensions: None,
        };

        let header = encode_header(&challenge).unwrap();
        let decoded: PaymentRequired = decode_header("PAYMENT-REQUIRED", &header).unwrap();
        assert_eq!(decoded, challenge);
    }

    #[test]
    fn wire_field_names_are_camel_case() {
        // A regression here is invisible in Rust and fatal on the wire.
        let json = serde_json::to_value(requirements()).unwrap();
        assert!(json.get("payTo").is_some(), "must serialize as payTo");
        assert!(json.get("pay_to").is_none());
        assert_eq!(json["maxTimeoutSeconds"], 60);
    }

    #[test]
    fn accepts_a_payment_that_covers_the_price() {
        let expected = requirements();
        let payload = payload_for(expected.clone(), "1000");
        assert!(payload.check_against(&expected).is_ok());
    }

    #[test]
    fn accepts_an_overpayment_and_a_checksummed_address() {
        let expected = requirements();
        let mut accepted = expected.clone();
        accepted.pay_to = accepted.pay_to.to_uppercase().replace("0X", "0x");
        let payload = payload_for(accepted, "5000");
        assert!(payload.check_against(&expected).is_ok());
    }

    #[test]
    fn rejects_undercutting_the_quoted_price() {
        let expected = requirements();
        // Both the echo and the signed value have to cover the price; one is
        // enough to make the payment worth less than the route.
        let mut accepted = expected.clone();
        accepted.amount = "1".to_owned();
        let payload = payload_for(accepted, "1");
        assert!(matches!(
            payload.check_against(&expected),
            Err(ProtocolError::AmountTooLow { .. })
        ));
    }

    #[test]
    fn rejects_paying_a_different_token_or_wallet() {
        let expected = requirements();

        let mut other_asset = expected.clone();
        other_asset.asset = "0x0000000000000000000000000000000000000001".to_owned();
        assert!(matches!(
            payload_for(other_asset, "1000").check_against(&expected),
            Err(ProtocolError::AssetMismatch)
        ));

        let mut other_wallet = expected.clone();
        other_wallet.pay_to = "0x9999999999999999999999999999999999999999".to_owned();
        assert!(matches!(
            payload_for(other_wallet, "1000").check_against(&expected),
            Err(ProtocolError::PayToMismatch { .. })
        ));
    }

    #[test]
    fn rejects_the_wrong_protocol_version() {
        let expected = requirements();
        let mut payload = payload_for(expected.clone(), "1000");
        payload.x402_version = 1;
        assert!(matches!(
            payload.check_against(&expected),
            Err(ProtocolError::VersionMismatch { found: 1, .. })
        ));
    }

    #[test]
    fn atomic_comparison_scales_past_u64() {
        // 10^18-1 vs 10^18: fits in u64, but 10^19 does not — so the compare
        // cannot go through an integer type.
        assert_eq!(
            compare_atomic("999999999999999999", "1000000000000000000"),
            Ordering::Less
        );
        assert_eq!(
            compare_atomic("10000000000000000000", "1000000000000000000"),
            Ordering::Greater
        );
        assert_eq!(compare_atomic("01000", "1000"), Ordering::Equal);
        assert_eq!(compare_atomic("0", "0"), Ordering::Equal);
    }

    #[test]
    fn broken_headers_are_reported_with_their_name() {
        assert!(matches!(
            decode_header::<PaymentRequired>("PAYMENT-REQUIRED", "not base64!!"),
            Err(ProtocolError::BadBase64 {
                header: "PAYMENT-REQUIRED",
                ..
            })
        ));
        assert!(matches!(
            decode_header::<PaymentRequired>("PAYMENT-REQUIRED", "e30="), // "{}"
            Err(ProtocolError::BadJson {
                header: "PAYMENT-REQUIRED",
                ..
            })
        ));
    }

    #[test]
    fn settlement_receipts_deserialize_from_the_facilitators_shape() {
        let raw = r#"{
            "success": true,
            "transaction": "0xfeed",
            "network": "eip155:2366",
            "payer": "0xabc",
            "amount": "1000"
        }"#;
        let receipt: SettleResponse = serde_json::from_str(raw).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.transaction, "0xfeed");

        // The failure shape keeps `transaction` (empty) — the schema requires it.
        let failed: SettleResponse = serde_json::from_str(
            r#"{"success":false,"errorReason":"insufficient_funds","transaction":"","network":"eip155:2366"}"#,
        )
        .unwrap();
        assert!(!failed.success);
        assert_eq!(failed.error_reason.as_deref(), Some("insufficient_funds"));
    }
}
