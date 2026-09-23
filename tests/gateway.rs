//! End-to-end tests: a real axum server on a real socket, a real facilitator
//! standing in for `facilitator.pieverse.io`, and a real upstream standing in
//! for whatever API is being wrapped.
//!
//! The unit tests inside `src/` check the pieces. These check the *seams* —
//! the base64 header round trip, the `/v1` path rewrite, the order of "call
//! upstream" versus "move money" — which is where a paywall actually leaks
//! money when it is wrong.
//!
//! Every test drives the service the way a client would: send a request, read
//! the 402, satisfy it, send it again. Nothing reaches into the gate directly.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tollgate::app::{AppState, serve};
use tollgate::chains::MAINNET;
use tollgate::facilitator::Facilitator;
use tollgate::manifest::Manifest;
use tollgate::paywall::Gate;
use tollgate::protocol::{
    ExactEvmPayload, PaymentPayload, PaymentRequired, PaymentRequirements, TransferAuthorization,
    decode_header, encode_header,
};
use tollgate::upstream::Upstream;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Wallet the manifest points at. Never a real one.
const PAY_TO: &str = "0x5A5a5A5a5A5a5A5a5A5a5A5a5A5a5A5a5A5a5A5a";

/// A caller, also invented.
const CALLER: &str = "0x1111111111111111111111111111111111111111";

/// The manifest every test runs against. Two endpoints at two prices, so a
/// test can prove the price actually varies by route rather than being a
/// constant that happens to match.
fn manifest_yaml(upstream_url: &str) -> String {
    format!(
        r#"
schema: 1
name: mock-weather
display_name: Mock Weather
description: A stand-in upstream used by the gateway's end to end tests.
maintainer:
  github: someone
status: draft
network: eip155:2366
pay_to: "{PAY_TO}"
upstream:
  name: Mock
  url: {upstream_url}
categories: [weather]
endpoints:
  - method: GET
    path: /v1/forecast
    summary: A forecast for one point on the globe.
    price_usd: "0.002"
    pitfalls:
      - latitude and longitude are required.
  - method: GET
    path: /v1/climate
    summary: A long range climate projection.
    price_usd: "0.010"
"#
    )
}

/// Everything a test needs to talk to the service under test.
struct Harness {
    origin: String,
    facilitator: MockServer,
    upstream: MockServer,
    client: reqwest::Client,
}

impl Harness {
    /// Stand up the mocks, then the gateway in front of them.
    async fn start() -> Self {
        let facilitator = MockServer::start().await;
        let upstream = MockServer::start().await;

        let manifest = Manifest::from_yaml_str("mock.yaml", &manifest_yaml(&upstream.uri()))
            .expect("the test manifest is valid");
        let routes = manifest
            .priced_routes(&MAINNET)
            .expect("prices are expressible on mainnet");

        let gate = Gate::new(routes, Facilitator::new(facilitator.uri()), &MAINNET);
        let state = Arc::new(AppState::new(
            gate,
            Upstream::new(upstream.uri(), "Mock", None),
        ));

        // Bind and drop, so the OS hands out a free port we can then name.
        // Racy in principle; on a loopback ephemeral port, in a test, never in
        // practice.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("can bind a probe port");
            probe
                .local_addr()
                .expect("probe port has an address")
                .port()
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        tokio::spawn(async move {
            let _ = serve(state, addr).await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test http client builds");

        let harness = Self {
            origin: format!("http://{addr}"),
            facilitator,
            upstream,
            client,
        };
        harness.await_ready().await;
        harness
    }

    /// Poll `/healthz` until the server answers, so tests never race the
    /// listener.
    async fn await_ready(&self) {
        for _ in 0..200 {
            if self
                .client
                .get(format!("{}/healthz", self.origin))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the gateway never started listening");
    }

    /// Point `/verify` at a canned answer.
    async fn verify_says(&self, body: Value) {
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.facilitator)
            .await;
    }

    /// Point `/settle` at a canned answer.
    async fn settle_says(&self, body: Value) {
        Mock::given(method("POST"))
            .and(path("/settle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.facilitator)
            .await;
    }

    /// Point the upstream at a canned answer for `path`.
    async fn upstream_says(&self, route_path: &str, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path(route_path))
            .respond_with(
                ResponseTemplate::new(status).set_body_raw(body.to_owned(), "application/json"),
            )
            .mount(&self.upstream)
            .await;
    }

    /// How many requests the facilitator received on `endpoint`.
    async fn facilitator_hits(&self, endpoint: &str) -> usize {
        self.facilitator
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == endpoint)
            .count()
    }

    /// How many requests reached the upstream.
    async fn upstream_hits(&self) -> usize {
        self.upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .len()
    }

    /// Every request the upstream saw, as (path, headers) pairs.
    async fn upstream_requests(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|request| {
                let headers = request
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_ascii_lowercase(),
                            value.to_str().unwrap_or("<binary>").to_owned(),
                        )
                    })
                    .collect();
                (request.url.path().to_owned(), headers)
            })
            .collect()
    }

    /// The bodies the facilitator received on `endpoint`, parsed.
    async fn facilitator_bodies(&self, endpoint: &str) -> Vec<Value> {
        self.facilitator
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == endpoint)
            .map(|request| serde_json::from_slice(&request.body).expect("the facilitator got JSON"))
            .collect()
    }

    /// Fetch a quote the way a client would, and return the entry to satisfy.
    async fn quote(&self, route_path: &str) -> PaymentRequirements {
        let response = self
            .client
            .get(format!("{}{route_path}", self.origin))
            .send()
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), 402, "an unpaid call must be quoted");

        let header = response
            .headers()
            .get("PAYMENT-REQUIRED")
            .expect("a 402 carries a PAYMENT-REQUIRED header")
            .to_str()
            .expect("the quote header is ASCII")
            .to_owned();

        let challenge: PaymentRequired =
            decode_header("PAYMENT-REQUIRED", &header).expect("the quote decodes");
        assert_eq!(challenge.x402_version, 2);
        challenge.accepts.into_iter().next().expect("one option")
    }
}

/// Build a signed-looking payment for `requirements`. No real key is involved:
/// the facilitator we talk to is a mock, so the signature only has to be the
/// right shape.
fn pay_for(requirements: &PaymentRequirements) -> String {
    pay_amount(requirements, &requirements.amount)
}

/// The same, but paying `value` instead — for tests that underpay.
fn pay_amount(requirements: &PaymentRequirements, value: &str) -> String {
    let payment = PaymentPayload {
        x402_version: 2,
        resource: None,
        accepted: PaymentRequirements {
            amount: value.to_owned(),
            ..requirements.clone()
        },
        payload: ExactEvmPayload {
            authorization: TransferAuthorization {
                from: CALLER.to_owned(),
                to: requirements.pay_to.clone(),
                value: value.to_owned(),
                valid_after: "0".to_owned(),
                valid_before: "4102444800".to_owned(),
                nonce: format!("0x{}", "7f".repeat(32)),
            },
            signature: format!("0x{}", "cd".repeat(65)),
            authorization_type: None,
        },
        extensions: None,
    };
    encode_header(&payment).expect("a payment header encodes")
}

/// A `delay` body for `/verify` that accepts.
fn verified() -> Value {
    json!({ "isValid": true, "payer": CALLER })
}

/// A `/settle` body that moved money.
fn settled() -> Value {
    json!({
        "success": true,
        "payer": CALLER,
        "transaction": "0x9f2c4b1e7a3d5081c6b9e2f4a7d0c3b6e9f2a5d8c1b4e7f0a3d6c9b2e5f8a1d4",
        "network": "eip155:2366",
        "amount": "2000"
    })
}

// ---------------------------------------------------------------------------
// Free surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_publishes_the_price_list_for_free() {
    let harness = Harness::start().await;

    let response = harness
        .client
        .get(format!("{}/healthz", harness.origin))
        .send()
        .await
        .expect("healthz answers");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("healthz is JSON");

    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["network"], json!("eip155:2366"));
    assert_eq!(body["asset"], json!("USDC.e"));

    let routes = body["routes"].as_array().expect("a route list");
    assert_eq!(routes.len(), 2);

    let forecast = routes
        .iter()
        .find(|route| route["path"] == json!("/v1/forecast"))
        .expect("forecast is listed");
    assert_eq!(forecast["price"], json!("0.002 USDC.e"));
    assert_eq!(forecast["amount"], json!("2000"));

    // Reading the price list must not have cost anything or called anyone.
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
    assert_eq!(harness.upstream_hits().await, 0);
}

#[tokio::test]
async fn a_path_the_manifest_does_not_price_is_not_gated() {
    let harness = Harness::start().await;
    harness
        .upstream_says("/meta", 200, r#"{"version":3}"#)
        .await;

    let response = harness
        .client
        .get(format!("{}/v1/meta", harness.origin))
        .send()
        .await
        .expect("the gateway answers");

    // Not 402: an endpoint added upstream tomorrow is reachable today, and
    // forgetting to price it gives the data away rather than breaking the API.
    assert_eq!(response.status(), 200);
    assert_eq!(harness.upstream_hits().await, 1, "relayed untouched");
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
}

// ---------------------------------------------------------------------------
// The quote
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_quote_is_issued_without_asking_anyone() {
    let harness = Harness::start().await;

    let requirements = harness.quote("/v1/forecast").await;

    assert_eq!(requirements.scheme, "exact");
    assert_eq!(requirements.network, "eip155:2366");
    assert_eq!(requirements.amount, "2000", "0.002 USDC.e");
    assert_eq!(requirements.pay_to, PAY_TO);
    assert_eq!(
        requirements.extra.get("name").and_then(Value::as_str),
        Some("Bridged USDC (Kite AI)"),
        "the EIP-712 domain has to ride along or the signature is unusable"
    );

    // Quoting is local arithmetic. Neither the facilitator nor the upstream
    // should have been woken up for it.
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
    assert_eq!(harness.upstream_hits().await, 0);
}

#[tokio::test]
async fn each_route_is_quoted_at_its_own_price() {
    let harness = Harness::start().await;

    assert_eq!(harness.quote("/v1/forecast").await.amount, "2000");
    assert_eq!(harness.quote("/v1/climate").await.amount, "10000");
}

#[tokio::test]
async fn the_quote_names_the_url_the_caller_actually_used() {
    let harness = Harness::start().await;

    let response = harness
        .client
        .get(format!(
            "{}/v1/forecast?latitude=52.52&longitude=13.41",
            harness.origin
        ))
        .send()
        .await
        .expect("the gateway answers");

    let header = response.headers()["PAYMENT-REQUIRED"]
        .to_str()
        .expect("ASCII")
        .to_owned();
    let challenge: PaymentRequired = decode_header("PAYMENT-REQUIRED", &header).unwrap();

    // A client signs over the resource it was quoted. If the query string were
    // dropped here, every quote for a parameterised endpoint would be for a
    // URL nobody asked for.
    assert!(
        challenge
            .resource
            .url
            .ends_with("/v1/forecast?latitude=52.52&longitude=13.41")
    );
    assert_eq!(
        challenge.resource.description.as_deref(),
        Some("A forecast for one point on the globe.")
    );
}

// ---------------------------------------------------------------------------
// Paying
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_paid_call_returns_the_data_and_a_receipt() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness
        .upstream_says("/forecast", 200, r#"{"temperature":18.5}"#)
        .await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast?latitude=52.52", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 200);
    let receipt = response.headers()["PAYMENT-RESPONSE"]
        .to_str()
        .expect("the receipt header is ASCII")
        .to_owned();
    let body: Value = response
        .json()
        .await
        .expect("the upstream body passes through");
    assert_eq!(body["temperature"], json!(18.5));

    let settled: tollgate::protocol::SettleResponse =
        decode_header("PAYMENT-RESPONSE", &receipt).expect("the receipt decodes");
    assert!(settled.success);
    assert_eq!(settled.network, "eip155:2366");

    assert_eq!(harness.facilitator_hits("/verify").await, 1);
    assert_eq!(harness.facilitator_hits("/settle").await, 1);
    assert_eq!(harness.upstream_hits().await, 1);

    // The `/v1` prefix belongs to the wrapper, not the upstream.
    let requests = harness.upstream_requests().await;
    assert_eq!(requests[0].0, "/forecast");
}

#[tokio::test]
async fn the_settlement_asks_for_the_price_the_caller_was_quoted() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness.upstream_says("/climate", 200, "{}").await;

    let requirements = harness.quote("/v1/climate").await;
    let response = harness
        .client
        .get(format!("{}/v1/climate", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), 200);

    let bodies = harness.facilitator_bodies("/settle").await;
    assert_eq!(bodies.len(), 1);
    let requirements_sent = &bodies[0]["paymentRequirements"];
    assert_eq!(
        requirements_sent["amount"],
        json!("10000"),
        "settling for less than the quoted price is how a paywall gets drained"
    );
    assert_eq!(requirements_sent["payTo"], json!(PAY_TO));
    assert_eq!(bodies[0]["x402Version"], json!(2));
}

#[tokio::test]
async fn both_spellings_of_the_payment_header_are_accepted() {
    // The v1 header name is still in circulation. Accepting it costs nothing
    // and refusing it turns a working client into a support ticket.
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness.upstream_says("/forecast", 200, "{}").await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("X-PAYMENT", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 200);
    assert_eq!(harness.facilitator_hits("/settle").await, 1);
}

// ---------------------------------------------------------------------------
// Refusing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_underpaying_client_is_refused_before_anything_is_relayed() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.upstream_says("/forecast", 200, "{}").await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_amount(&requirements, "1"))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 402);
    let challenge: PaymentRequired = decode_header(
        "PAYMENT-REQUIRED",
        response.headers()["PAYMENT-REQUIRED"].to_str().unwrap(),
    )
    .unwrap();
    let reason = challenge.error.expect("the 402 says what was wrong");
    assert!(reason.contains("costs"), "reason was {reason:?}");

    // Nothing was asked of anyone: the mismatch is visible locally.
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
    assert_eq!(harness.upstream_hits().await, 0);
}

#[tokio::test]
async fn a_payment_to_the_wrong_wallet_is_refused() {
    // The attack the local check exists for: a well-formed authorization that
    // pays somebody else, presented for a route that pays us.
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.upstream_says("/forecast", 200, "{}").await;

    // A consistent, well-formed payment — just addressed to somebody else. The
    // echoed `accepted` and the signed `to` agree with each other, so only the
    // server's own comparison against its manifest can catch it.
    let echoed = harness.quote("/v1/forecast").await;
    let payment = PaymentPayload {
        x402_version: 2,
        resource: None,
        accepted: PaymentRequirements {
            pay_to: "0x000000000000000000000000000000000000dEaD".to_owned(),
            ..echoed.clone()
        },
        payload: ExactEvmPayload {
            authorization: TransferAuthorization {
                from: CALLER.to_owned(),
                to: "0x000000000000000000000000000000000000dEaD".to_owned(),
                value: echoed.amount.clone(),
                valid_after: "0".to_owned(),
                valid_before: "4102444800".to_owned(),
                nonce: format!("0x{}", "11".repeat(32)),
            },
            signature: format!("0x{}", "ab".repeat(65)),
            authorization_type: None,
        },
        extensions: None,
    };

    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", encode_header(&payment).unwrap())
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 402);
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
    assert_eq!(harness.upstream_hits().await, 0);
}

#[tokio::test]
async fn a_garbage_header_is_explained_rather_than_ignored() {
    let harness = Harness::start().await;

    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", "not-base64-at-all!!!")
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 402);
    let challenge: PaymentRequired = decode_header(
        "PAYMENT-REQUIRED",
        response.headers()["PAYMENT-REQUIRED"].to_str().unwrap(),
    )
    .unwrap();
    assert!(challenge.error.is_some(), "the caller deserves a reason");
    assert_eq!(harness.facilitator_hits("/verify").await, 0);
}

#[tokio::test]
async fn a_payment_the_facilitator_rejects_never_reaches_the_upstream() {
    let harness = Harness::start().await;
    harness
        .verify_says(json!({
            "isValid": false,
            "invalidReason": "insufficient_funds",
            "invalidMessage": "the account cannot cover the authorization"
        }))
        .await;
    harness
        .upstream_says("/forecast", 200, r#"{"temperature":18.5}"#)
        .await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 402);
    let challenge: PaymentRequired = decode_header(
        "PAYMENT-REQUIRED",
        response.headers()["PAYMENT-REQUIRED"].to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(
        challenge.error.as_deref(),
        Some("the account cannot cover the authorization"),
        "the facilitator's own words are more useful than ours"
    );

    // The upstream provider is not billed for a call nobody paid for.
    assert_eq!(harness.upstream_hits().await, 0);
    assert_eq!(harness.facilitator_hits("/settle").await, 0);
}

// ---------------------------------------------------------------------------
// The two ways to lose money
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upstream_failure_is_not_charged_for() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness
        .upstream_says("/forecast", 503, r#"{"error":"upstream is down"}"#)
        .await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    // The caller sees the upstream's problem, and keeps their money.
    assert_eq!(response.status(), 503);
    assert_eq!(
        harness.facilitator_hits("/settle").await,
        0,
        "verifying a payment is fine; moving it for a call that failed is not"
    );
    assert!(
        response.headers().get("PAYMENT-RESPONSE").is_none(),
        "no settlement, no receipt"
    );
}

#[tokio::test]
async fn a_missing_upstream_parameter_is_not_charged_for() {
    // A 4xx is the caller's mistake, but it is still not a product. Charging
    // for a 422 would be taking money and returning an error message.
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness
        .upstream_says("/forecast", 400, r#"{"error":"latitude is required"}"#)
        .await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 400);
    assert_eq!(harness.facilitator_hits("/settle").await, 0);
}

#[tokio::test]
async fn a_settlement_that_fails_withholds_the_data() {
    // The inverse leak: verify said yes, the upstream returned data, and then
    // the transfer did not land. Serving the body here would be giving the
    // product away, so the caller gets a 402 and nothing else.
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness
        .settle_says(json!({
            "success": false,
            "errorReason": "insufficient_funds",
            "errorMessage": "the transfer was rejected by the token",
            "transaction": "",
            "network": "eip155:2366"
        }))
        .await;
    harness
        .upstream_says(
            "/forecast",
            200,
            r#"{"temperature":18.5,"secret":"paid data"}"#,
        )
        .await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), 402);
    let text = response.text().await.expect("a body to read");
    assert!(
        !text.contains("paid data"),
        "the upstream payload must not appear in a failed settlement: {text}"
    );
    assert!(text.contains("the transfer was rejected by the token"));

    // The call was made and wasted. That is the cost of settling after the
    // fact, and it is cheaper than charging for nothing.
    assert_eq!(harness.upstream_hits().await, 1);
}

// ---------------------------------------------------------------------------
// Our own outages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_facilitator_outage_is_our_problem_not_the_callers() {
    let harness = Harness::start().await;
    Mock::given(method("POST"))
        .and(path("/verify"))
        .respond_with(ResponseTemplate::new(500).set_body_string("facilitator exploded"))
        .mount(&harness.facilitator)
        .await;
    harness.upstream_says("/forecast", 200, "{}").await;

    let requirements = harness.quote("/v1/forecast").await;
    let response = harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .expect("the gateway answers");

    // 502, emphatically not 402. Telling a caller who did nothing wrong to pay
    // again is how the same payment gets signed twice.
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["operation"], json!("verify"));
    assert_eq!(body["error"], json!("facilitator unavailable"));
    assert_eq!(harness.upstream_hits().await, 0);
}

#[tokio::test]
async fn the_manifest_is_the_only_thing_that_decides_what_is_for_sale() {
    // Nothing is hardcoded: the price list in `/healthz` is the manifest, the
    // route table is the manifest, and the two cannot disagree.
    let harness = Harness::start().await;

    let body: Value = harness
        .client
        .get(format!("{}/healthz", harness.origin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let paths: Vec<&str> = body["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| route["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["/v1/forecast", "/v1/climate"]);
}

#[tokio::test]
async fn the_meter_counts_what_happened() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness.upstream_says("/forecast", 200, "{}").await;
    harness.upstream_says("/climate", 500, "{}").await;

    // One winning call.
    let requirements = harness.quote("/v1/forecast").await;
    harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .unwrap();

    // One call that got as far as the upstream and lost there.
    let requirements = harness.quote("/v1/climate").await;
    harness
        .client
        .get(format!("{}/v1/climate", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .unwrap();

    // One call that never had a payment at all.
    harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .send()
        .await
        .unwrap();

    let body: Value = harness
        .client
        .get(format!("{}/healthz", harness.origin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Two quotes were issued for calls that carried no payment, so three
    // challenges in total across the run.
    assert_eq!(body["meter"]["settled"], json!(1));
    assert_eq!(body["meter"]["refused"], json!(1));
    assert_eq!(body["meter"]["challenged"], json!(3));
    assert_eq!(body["meter"]["upstream_errors"], json!(0));
}

// ---------------------------------------------------------------------------
// Not leaking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_signed_authorization_stays_here() {
    let harness = Harness::start().await;
    harness.verify_says(verified()).await;
    harness.settle_says(settled()).await;
    harness.upstream_says("/forecast", 200, "{}").await;

    let requirements = harness.quote("/v1/forecast").await;
    harness
        .client
        .get(format!("{}/v1/forecast", harness.origin))
        .header("PAYMENT-SIGNATURE", pay_for(&requirements))
        .send()
        .await
        .unwrap();

    let requests = harness.upstream_requests().await;
    let (_, headers) = &requests[0];
    let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();

    // A relayed authorization is a bearer instrument for the payer's money:
    // whoever holds it can present it to the facilitator themselves.
    assert!(
        !names.iter().any(|name| name.contains("payment")),
        "payment headers leaked upstream: {names:?}"
    );
    // Hop-by-hop headers describe a connection the upstream is not on.
    for hop_by_hop in ["connection", "keep-alive", "transfer-encoding", "upgrade"] {
        assert!(
            !names.contains(&hop_by_hop),
            "{hop_by_hop} must not be forwarded: {names:?}"
        );
    }
}
