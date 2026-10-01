//! Error types.
//!
//! Every fallible step in the payment path gets its own enum instead of one
//! crate-wide `Error`. The paywall has to make different decisions per class:
//! a price that does not parse is a boot-time problem (refuse to start), a
//! malformed signature is a 402, and an unreachable facilitator is a 502.
//! Collapsing them into one type loses exactly the information the caller
//! needs to answer correctly.

use thiserror::Error;

/// A price in the manifest could not be turned into atomic token units.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PriceError {
    #[error("price {input:?} is not a plain decimal number")]
    NotADecimal { input: String },

    #[error(
        "price {input:?} has {found} decimal places but the settling token allows only {allowed}"
    )]
    TooPrecise {
        input: String,
        found: usize,
        allowed: usize,
    },

    #[error("price {input:?} rounds down to zero atomic units")]
    RoundsToZero { input: String },
}

/// A header or body that is supposed to be x402 v2 did not match the wire shape.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("{header} header is not valid base64: {source}")]
    BadBase64 {
        header: &'static str,
        #[source]
        source: base64::DecodeError,
    },

    #[error("{header} header is not valid JSON: {source}")]
    BadJson {
        header: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("payment payload claims x402Version {found}, this server speaks {expected}")]
    VersionMismatch { found: u8, expected: u8 },

    #[error("payment payload does not carry a `payload` object with an `authorization`")]
    MissingAuthorization,

    #[error("payment does not authorize the asset this route settles in")]
    AssetMismatch,

    #[error("payment authorizes {found} but the route costs {expected}")]
    AmountTooLow { found: String, expected: String },

    #[error("payment is addressed to {found}, expected {expected}")]
    PayToMismatch { found: String, expected: String },
}

/// The manifest file is not something this server can run.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid YAML: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("manifest declares schema {found}, this build understands schema 1")]
    UnsupportedSchema { found: i64 },

    #[error("{path} declares no endpoints to charge for")]
    NoEndpoints { path: String },

    #[error("endpoint {method} {path} declares price {price:?}: {source}")]
    BadPrice {
        method: String,
        path: String,
        price: String,
        #[source]
        source: PriceError,
    },

    #[error("endpoint {method} {path} appears twice in the manifest")]
    DuplicateEndpoint { method: String, path: String },

    #[error(
        "manifest network {network:?} is not a Kite chain this build knows (eip155:2366, eip155:2368)"
    )]
    UnknownNetwork { network: String },

    #[error("pay_to {pay_to:?} is not a 0x-prefixed 20-byte address")]
    BadPayTo { pay_to: String },

    #[error("upstream url {url:?} must be an absolute http(s) url")]
    BadUpstreamUrl { url: String },
}

/// The facilitator refused to answer, or answered with something unusable.
#[derive(Debug, Error)]
pub enum FacilitatorError {
    #[error("facilitator {operation} request to {url} failed: {source}")]
    Transport {
        operation: &'static str,
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("facilitator {operation} returned HTTP {status}: {body}")]
    Status {
        operation: &'static str,
        status: u16,
        body: String,
    },

    #[error("facilitator {operation} returned a body that is not the expected JSON: {source}")]
    Decode {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl FacilitatorError {
    /// Whether a second attempt could succeed where this one failed.
    ///
    /// Transport failures (connection refused, reset, timeout) are classic
    /// transients. A 5xx from the facilitator means "I am having trouble",
    /// which also earns a retry — the settle payload is an authorization with
    /// a nonce, so a re-submission of the *same* payment cannot spend twice;
    /// it either lands or is rejected as a replay. A 4xx is the facilitator
    /// making a statement about *this* payment, and a decode error means we
    /// did get an answer we failed to understand: both are deterministic, and
    /// repeating them is just a slower way to fail.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Status { status, .. } => (500..600).contains(status),
            Self::Decode { .. } => false,
        }
    }
}

/// Failure while relaying the request to the upstream API.
#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("cannot reach {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("upstream returned {status} for {url}")]
    Status { url: String, status: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_5xx_are_retryable_4xx_and_decode_are_not() {
        // The Transport branch cannot be constructed here (reqwest keeps its
        // error constructor private); the closed-port tests in paywall.rs
        // exercise it with the real thing.

        for status in [500u16, 502, 503, 599] {
            let server_error = FacilitatorError::Status {
                operation: "settle",
                status,
                body: "<empty body>".to_owned(),
            };
            assert!(server_error.is_retryable(), "{status} should retry");
        }

        for status in [400u16, 401, 404, 429] {
            let caller_error = FacilitatorError::Status {
                operation: "settle",
                status,
                body: "<empty body>".to_owned(),
            };
            assert!(!caller_error.is_retryable(), "{status} must not retry");
        }
    }
}
