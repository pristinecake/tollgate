//! HTTP client for the x402 facilitator.
//!
//! The facilitator is the only piece of x402 that cannot run inside the
//! service: it holds the relayer key that actually moves the stablecoin, so
//! `verify` and `settle` are remote calls. Three endpoints, three JSON bodies:
//!
//! | call | method | body |
//! |---|---|---|
//! | `/supported` | GET | — |
//! | `/verify` | POST | `{x402Version, paymentPayload, paymentRequirements}` |
//! | `/settle` | POST | same shape as `/verify` |
//!
//! Only `/settle` moves money. [`Facilitator::verify`] is a question, and this
//! service asks it before touching the upstream so a bad signature never costs
//! the upstream provider a call.

use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};

use crate::error::FacilitatorError;
use crate::protocol::{
    PaymentPayload, PaymentRequirements, SettleResponse, SupportedResponse, VerifyRequest,
    VerifyResponse, X402_VERSION,
};

/// How long to wait on the facilitator before giving up on a paid request.
///
/// Generous, because a `settle` is an on-chain submission: too short a timeout
/// would abandon requests that were about to succeed, and the retry would need
/// a fresh authorization from the client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Longest error body kept for a log line.
const ERROR_EXCERPT_LIMIT: usize = 240;

/// A handle on one facilitator deployment.
#[derive(Debug, Clone)]
pub struct Facilitator {
    base_url: String,
    client: reqwest::Client,
}

impl Facilitator {
    /// Build a client for `base_url`, keeping Kite's default HTTP settings.
    ///
    /// # Panics
    ///
    /// Panics only if the TLS backend fails to initialise, which is a build
    /// problem rather than a runtime condition.
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client should build with the rustls feature enabled");
        Self::with_client(base_url, client)
    }

    /// Build a client around a caller-supplied [`reqwest::Client`], so tests can
    /// inject timeouts, proxies, or instrumentation.
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            // Trailing slashes would produce `//verify`, which some reverse
            // proxies redirect and others reject.
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// The facilitator this handle talks to, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /supported` — which schemes and networks this facilitator services.
    ///
    /// # Errors
    ///
    /// [`FacilitatorError`] when the facilitator is unreachable, answers with a
    /// non-2xx status, or returns a body that is not a `SupportedResponse`.
    pub async fn supported(&self) -> Result<SupportedResponse, FacilitatorError> {
        let url = format!("{}/supported", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|source| self.transport("supported", &url, source))?;

        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|source| self.transport("supported", &url, source))?;

        if !status.is_success() {
            return Err(FacilitatorError::Status {
                operation: "supported",
                status: status.as_u16(),
                body: excerpt(&body),
            });
        }

        serde_json::from_slice(&body).map_err(|source| FacilitatorError::Decode {
            operation: "supported",
            source,
        })
    }

    /// `POST /verify` — ask whether a payment is spendable.
    ///
    /// A `isValid: false` answer is a *successful* call with a negative result:
    /// it is returned as `Ok`, and the caller turns it into a 402. Only
    /// transport, status, and decoding failures are `Err` — those are our
    /// problem (502), not the caller's.
    ///
    /// # Errors
    ///
    /// [`FacilitatorError`] on transport failure, non-2xx status, or a body
    /// that is not a `VerifyResponse`.
    pub async fn verify(
        &self,
        payment: &PaymentPayload,
        requirements: &PaymentRequirements,
    ) -> Result<VerifyResponse, FacilitatorError> {
        self.post(
            "verify",
            &VerifyRequest {
                x402_version: X402_VERSION,
                payment_payload: payment.clone(),
                payment_requirements: requirements.clone(),
            },
        )
        .await
    }

    /// `POST /settle` — move the money and return the receipt.
    ///
    /// # Errors
    ///
    /// [`FacilitatorError`] on transport failure, non-2xx status, or a body
    /// that is not a `SettleResponse`.
    pub async fn settle(
        &self,
        payment: &PaymentPayload,
        requirements: &PaymentRequirements,
    ) -> Result<SettleResponse, FacilitatorError> {
        self.post(
            "settle",
            &VerifyRequest {
                x402_version: X402_VERSION,
                payment_payload: payment.clone(),
                payment_requirements: requirements.clone(),
            },
        )
        .await
    }

    async fn post<T, R>(&self, operation: &'static str, body: &T) -> Result<R, FacilitatorError>
    where
        T: Serialize + Sync,
        R: DeserializeOwned,
    {
        let url = format!("{}/{operation}", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|source| self.transport(operation, &url, source))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|source| self.transport(operation, &url, source))?;

        if !status.is_success() {
            return Err(FacilitatorError::Status {
                operation,
                status: status.as_u16(),
                body: excerpt(&bytes),
            });
        }

        serde_json::from_slice(&bytes)
            .map_err(|source| FacilitatorError::Decode { operation, source })
    }

    fn transport(
        &self,
        operation: &'static str,
        url: &str,
        source: reqwest::Error,
    ) -> FacilitatorError {
        FacilitatorError::Transport {
            operation,
            url: url.to_owned(),
            source,
        }
    }
}

/// Trim a body down to something safe to put in a log line or a 502 payload.
fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "<empty body>".to_owned();
    }
    if compact.chars().count() <= ERROR_EXCERPT_LIMIT {
        return compact;
    }
    let head: String = compact.chars().take(ERROR_EXCERPT_LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_slashes_so_paths_do_not_double_up() {
        assert_eq!(
            Facilitator::new("https://facilitator.example/v2///").base_url(),
            "https://facilitator.example/v2"
        );
    }

    #[test]
    fn excerpts_collapse_whitespace_and_clip() {
        assert_eq!(excerpt(b"  hello \n world  "), "hello world");
        assert_eq!(excerpt(b""), "<empty body>");
        assert_eq!(excerpt(b"   "), "<empty body>");

        let long = "x".repeat(500);
        let clipped = excerpt(long.as_bytes());
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), ERROR_EXCERPT_LIMIT + 1);
    }
}
