//! The paywall, as a decision function.
//!
//! There are no `axum`, `http`, or `tower` types in this file on purpose. The
//! interesting part of a paywall is *which* of four things should happen to a
//! request, and that question is easier to test — and to reason about during an
//! incident — when it is not tangled up with a router.
//!
//! The decision tree, in full:
//!
//! ```text
//!   route not priced                     -> Free         (no 402 at all)
//!   no PAYMENT-SIGNATURE header          -> Challenge    (402 + quote)
//!   header that will not decode          -> Challenge    (402 + reason)
//!   payment that does not cover the price-> Challenge    (402 + reason)
//!   facilitator unreachable              -> FacilitatorDown (502, caller's fault: nobody's)
//!   facilitator says invalid             -> Challenge    (402 + reason)
//!   facilitator says valid               -> Paid         (run the handler)
//! ```
//!
//! `Free` is checked first, so a `/healthz` on a service that also sells data
//! never accidentally becomes a paid route.

use crate::chains::Chain;
use crate::error::{FacilitatorError, ProtocolError};
use crate::facilitator::Facilitator;
use crate::manifest::PricedRoute;
use crate::protocol::{
    HEADER_PAYMENT_SIGNATURE, HEADER_X_PAYMENT, PaymentPayload, PaymentRequired,
    PaymentRequirements, ResourceInfo, SCHEME_EXACT, SettleResponse, X402_VERSION, decode_header,
};

/// What the gate decided should happen to a request.
#[derive(Debug)]
pub enum Decision {
    /// The route is not for sale. Hand it to the upstream untouched.
    Free,

    /// Quoting a price. Send the challenge and stop.
    Challenge(Box<PaymentRequired>),

    /// Paid and verified. Run the handler, then settle.
    Paid {
        payment: Box<PaymentPayload>,
        requirements: Box<PaymentRequirements>,
        /// Address that signed, when the facilitator told us. Useful in logs.
        payer: Option<String>,
    },

    /// The facilitator could not be reached. This is a 502, not a 402: the
    /// caller did nothing wrong and should not be told to pay again.
    FacilitatorDown {
        operation: &'static str,
        detail: String,
    },
}

/// The gate itself: a route table plus a facilitator handle.
#[derive(Debug, Clone)]
pub struct Gate {
    routes: Vec<PricedRoute>,
    facilitator: Facilitator,
    chain: &'static Chain,
}

impl Gate {
    /// Build a gate over a priced route table.
    pub fn new(routes: Vec<PricedRoute>, facilitator: Facilitator, chain: &'static Chain) -> Self {
        Self {
            routes,
            facilitator,
            chain,
        }
    }

    /// The route table, in manifest order.
    pub fn routes(&self) -> &[PricedRoute] {
        &self.routes
    }

    /// The chain every route settles on.
    pub fn chain(&self) -> &'static Chain {
        self.chain
    }

    /// The facilitator this gate talks to.
    pub fn facilitator(&self) -> &Facilitator {
        &self.facilitator
    }

    /// Find the priced route for a method/path pair.
    ///
    /// Matching is exact on the path and case-insensitive on the method. Query
    /// strings are the caller's business and are ignored here — the price is a
    /// property of the endpoint, not of the parameters.
    pub fn route_for(&self, method: &str, path: &str) -> Option<&PricedRoute> {
        self.routes
            .iter()
            .find(|route| route.method.eq_ignore_ascii_case(method) && route.path == path)
    }

    /// Build the 402 body for `route`.
    pub fn challenge(
        &self,
        route: &PricedRoute,
        resource_url: &str,
        reason: Option<String>,
    ) -> PaymentRequired {
        PaymentRequired {
            x402_version: X402_VERSION,
            error: reason,
            resource: ResourceInfo {
                url: resource_url.to_owned(),
                description: Some(route.summary.clone()),
                mime_type: Some("application/json".to_owned()),
            },
            accepts: vec![route.requirements.clone()],
            extensions: None,
        }
    }

    /// The whole decision, for one request.
    ///
    /// `payment_header` is the raw `PAYMENT-SIGNATURE` value (or the v1
    /// `X-PAYMENT` spelling), base64 as it arrived.
    ///
    /// # Errors
    ///
    /// Does not return `Err`: an unreachable facilitator is a
    /// [`Decision::FacilitatorDown`], because it is a normal outcome that has to
    /// be turned into a response rather than propagated.
    pub async fn evaluate(
        &self,
        method: &str,
        path: &str,
        resource_url: &str,
        payment_header: Option<&str>,
    ) -> Decision {
        let Some(route) = self.route_for(method, path) else {
            return Decision::Free;
        };

        let Some(raw) = payment_header.filter(|raw| !raw.trim().is_empty()) else {
            return Decision::Challenge(Box::new(self.challenge(route, resource_url, None)));
        };

        // Everything up to the facilitator call is local, and refusing locally
        // is both faster and cheaper than asking a remote service about a
        // payment that is wrong on its face.
        let payment: PaymentPayload = match decode_header(HEADER_PAYMENT_SIGNATURE, raw) {
            Ok(payment) => payment,
            Err(error) => {
                return Decision::Challenge(Box::new(self.challenge(
                    route,
                    resource_url,
                    Some(describe(&error)),
                )));
            }
        };

        if let Err(error) = payment.check_against(&route.requirements) {
            return Decision::Challenge(Box::new(self.challenge(
                route,
                resource_url,
                Some(describe(&error)),
            )));
        }

        match self.facilitator.verify(&payment, &route.requirements).await {
            Ok(verified) if verified.is_valid => Decision::Paid {
                payer: verified
                    .payer
                    .or_else(|| Some(payment.payload.authorization.payer().to_owned())),
                payment: Box::new(payment),
                requirements: Box::new(route.requirements.clone()),
            },
            Ok(verified) => {
                let reason = verified
                    .invalid_message
                    .or(verified.invalid_reason)
                    .unwrap_or_else(|| "payment rejected by facilitator".to_owned());
                Decision::Challenge(Box::new(self.challenge(route, resource_url, Some(reason))))
            }
            Err(error) => Decision::FacilitatorDown {
                operation: "verify",
                detail: error.to_string(),
            },
        }
    }

    /// Settle a verified payment and return the receipt.
    ///
    /// # Errors
    ///
    /// [`FacilitatorError`] when the facilitator cannot be reached or refuses
    /// to answer.
    pub async fn settle(
        &self,
        payment: &PaymentPayload,
        requirements: &PaymentRequirements,
    ) -> Result<SettleResponse, FacilitatorError> {
        self.facilitator.settle(payment, requirements).await
    }
}

/// Whether an upstream response is good enough to charge for.
///
/// The rule is deliberately "the upstream said it worked": a 4xx means the
/// caller asked for something that does not exist, a 5xx means the upstream is
/// broken, and charging for either is how a paid API acquires a reputation for
/// taking money and returning nothing.
pub fn should_settle(upstream_status: u16) -> bool {
    upstream_status < 400
}

/// Turn a protocol error into something worth putting in a 402.
fn describe(error: &ProtocolError) -> String {
    error.to_string()
}

/// Accepted spellings of the payment header, version 2 first.
pub const PAYMENT_HEADERS: [&str; 2] = [HEADER_PAYMENT_SIGNATURE, HEADER_X_PAYMENT];

/// The scheme this crate implements. Present so callers can assert on it.
pub const SUPPORTED_SCHEME: &str = SCHEME_EXACT;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::MAINNET;
    use crate::manifest::{Endpoint, Manifest, Upstream};

    fn manifest_with(price: &str) -> Manifest {
        Manifest {
            schema: 1,
            name: "demo".to_owned(),
            display_name: "Demo".to_owned(),
            description: "A demo service described at sufficient length.".to_owned(),
            maintainer: None,
            status: "draft".to_owned(),
            network: "eip155:2366".to_owned(),
            pay_to: "0x1234567890123456789012345678901234567890".to_owned(),
            base_url: None,
            upstream: Upstream {
                name: "Example".to_owned(),
                url: "https://example.com".to_owned(),
                requires_api_key: false,
                terms_url: None,
            },
            endpoints: vec![Endpoint {
                method: "GET".to_owned(),
                path: "/v1/thing".to_owned(),
                summary: "Returns a thing.".to_owned(),
                price_usd: price.to_owned(),
                example_request: None,
                pitfalls: Vec::new(),
            }],
            categories: vec!["data".to_owned()],
            tags: Vec::new(),
            source: None,
        }
    }

    fn gate(price: &str) -> Gate {
        let manifest = manifest_with(price);
        let routes = manifest.priced_routes(&MAINNET).unwrap();
        // Nothing in these tests reaches the network: the facilitator URL is
        // only dialled on the `Paid` path.
        Gate::new(routes, Facilitator::new("http://127.0.0.1:1"), &MAINNET)
    }

    #[tokio::test]
    async fn unpriced_paths_are_not_charged_for() {
        let gate = gate("0.002");
        assert!(matches!(
            gate.evaluate("GET", "/healthz", "http://x/healthz", None)
                .await,
            Decision::Free
        ));
    }

    #[tokio::test]
    async fn the_method_has_to_match() {
        let gate = gate("0.002");
        assert!(matches!(
            gate.evaluate("POST", "/v1/thing", "http://x/v1/thing", None)
                .await,
            Decision::Free
        ));
    }

    #[tokio::test]
    async fn an_unpaid_request_gets_a_quote() {
        let gate = gate("0.002");
        let Decision::Challenge(challenge) = gate
            .evaluate("GET", "/v1/thing", "https://api.test/v1/thing", None)
            .await
        else {
            panic!("expected a challenge");
        };

        assert_eq!(challenge.x402_version, 2);
        assert!(
            challenge.error.is_none(),
            "a first challenge has nothing to explain"
        );
        assert_eq!(challenge.accepts.len(), 1);
        assert_eq!(challenge.accepts[0].amount, "2000");
        assert_eq!(challenge.resource.url, "https://api.test/v1/thing");
        assert_eq!(
            challenge.resource.description.as_deref(),
            Some("Returns a thing.")
        );
    }

    #[tokio::test]
    async fn a_garbage_payment_header_is_explained_not_ignored() {
        let gate = gate("0.002");
        let Decision::Challenge(challenge) = gate
            .evaluate(
                "GET",
                "/v1/thing",
                "https://api.test/v1/thing",
                Some("!!!not base64!!!"),
            )
            .await
        else {
            panic!("expected a challenge");
        };
        let reason = challenge.error.expect("must say why");
        assert!(reason.contains("base64"), "reason was {reason:?}");
    }

    #[tokio::test]
    async fn an_underpaying_payment_is_refused_before_any_network_call() {
        // The facilitator URL points at a closed port; if this test passes,
        // the local check ran first, which is the point.
        let gate = gate("0.002");
        let requirements = gate.routes()[0].requirements.clone();

        let mut accepted = requirements.clone();
        accepted.amount = "1".to_owned();
        let payment = PaymentPayload {
            x402_version: 2,
            resource: None,
            accepted,
            payload: crate::protocol::ExactEvmPayload {
                authorization: crate::protocol::TransferAuthorization {
                    from: "0xabcabcabcabcabcabcabcabcabcabcabcabcabca".to_owned(),
                    to: requirements.pay_to.clone(),
                    value: "1".to_owned(),
                    valid_after: "0".to_owned(),
                    valid_before: "9999999999".to_owned(),
                    nonce: format!("0x{}", "00".repeat(32)),
                },
                signature: format!("0x{}", "ab".repeat(65)),
                authorization_type: None,
            },
            extensions: None,
        };
        let header = crate::protocol::encode_header(&payment).unwrap();

        let Decision::Challenge(challenge) = gate
            .evaluate(
                "GET",
                "/v1/thing",
                "https://api.test/v1/thing",
                Some(&header),
            )
            .await
        else {
            panic!("expected a challenge, not a settlement attempt");
        };
        assert!(challenge.error.unwrap().contains("costs"));
    }

    #[test]
    fn only_successful_upstream_responses_are_charged_for() {
        for status in [200, 201, 204, 301, 304, 399] {
            assert!(should_settle(status), "{status} should be chargeable");
        }
        for status in [400, 401, 404, 429, 500, 502, 503] {
            assert!(!should_settle(status), "{status} must not be chargeable");
        }
    }
}
