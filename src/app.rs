//! The axum layer.
//!
//! Thin on purpose: [`crate::paywall`] decides, this file renders. Every branch
//! below corresponds to exactly one [`Decision`] variant, so the mapping from
//! "what should happen" to "what the caller sees" can be read in one screen.
//!
//! Routing uses a single fallback handler rather than one registered route per
//! endpoint. That is what makes the manifest authoritative: adding an endpoint
//! is a config change, not a code change, and a route that is not in the
//! manifest cannot accidentally be served because someone forgot to remove it.

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::get,
};
use serde::Serialize;
use serde_json::json;

use crate::chains::Chain;
use crate::money::format_price;
use crate::paywall::{Decision, Gate, should_settle};
use crate::protocol::{
    HEADER_PAYMENT_REQUIRED, HEADER_PAYMENT_RESPONSE, PaymentPayload, PaymentRequired,
    PaymentRequirements, SettleResponse, encode_header,
};
use crate::upstream::{RelayRequest, Upstream};

/// Largest request body relayed upstream. Paid endpoints are data lookups, not
/// uploads; anything larger is a mistake or an attack.
const MAX_REQUEST_BODY: usize = 1024 * 1024;

/// Counters behind `/healthz`. Deliberately not a metrics framework: four
/// numbers that answer "is it working" without adding a dependency.
#[derive(Debug, Default)]
pub struct Meter {
    challenged: AtomicU64,
    settled: AtomicU64,
    refused: AtomicU64,
    upstream_errors: AtomicU64,
    facilitator_errors: AtomicU64,
}

/// A point-in-time copy of the counters.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MeterSnapshot {
    /// 402s handed out.
    pub challenged: u64,
    /// Payments that landed on chain.
    pub settled: u64,
    /// Verified payments that were not charged for, because the upstream failed.
    pub refused: u64,
    /// Upstream transport failures.
    pub upstream_errors: u64,
    /// Facilitator failures.
    pub facilitator_errors: u64,
}

impl Meter {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the counters.
    pub fn snapshot(&self) -> MeterSnapshot {
        MeterSnapshot {
            challenged: self.challenged.load(Ordering::Relaxed),
            settled: self.settled.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            facilitator_errors: self.facilitator_errors.load(Ordering::Relaxed),
        }
    }
}

/// Everything the handlers need.
#[derive(Debug)]
pub struct AppState {
    gate: Gate,
    upstream: Upstream,
    meter: Meter,
}

impl AppState {
    /// Assemble the state from a gate and an upstream.
    pub fn new(gate: Gate, upstream: Upstream) -> Self {
        Self {
            gate,
            upstream,
            meter: Meter::default(),
        }
    }

    /// The paywall.
    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// The API being wrapped.
    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// The counters.
    pub fn meter(&self) -> &Meter {
        &self.meter
    }

    /// The chain every route settles on.
    pub fn chain(&self) -> &'static Chain {
        self.gate.chain()
    }
}

/// Build the router.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(dispatch)
        .with_state(state)
}

/// Bind and serve until the process is stopped.
///
/// # Errors
///
/// Propagates bind and accept errors from the listener.
pub async fn serve(state: Arc<AppState>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, build_router(state)).await
}

/// Free liveness probe that doubles as a price list, so a buyer can see what
/// this service charges without paying to find out.
async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let chain = state.chain();
    let routes: Vec<serde_json::Value> = state
        .gate()
        .routes()
        .iter()
        .map(|route| {
            json!({
                "method": route.method,
                "path": route.path,
                "summary": route.summary,
                "price": format_price(&route.price_usd, chain.stablecoin.symbol),
                "amount": route.amount,
            })
        })
        .collect();

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "network": chain.network,
            "asset": chain.stablecoin.symbol,
            "upstream": state.upstream().base_url(),
            "routes": routes,
            "meter": state.meter().snapshot(),
        }),
    )
}

/// The one handler every request lands in.
async fn dispatch(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    let resource_url = resource_url(request.headers(), &path, query.as_deref());
    let payment = payment_header(request.headers());

    let decision = state
        .gate()
        .evaluate(&method, &path, &resource_url, payment.as_deref())
        .await;

    match decision {
        Decision::Free => forward(&state, request, &method, &path, query.as_deref(), None).await,

        Decision::Challenge(challenge) => {
            Meter::bump(&state.meter().challenged);
            challenge_response(&challenge)
        }

        Decision::FacilitatorDown { operation, detail } => {
            Meter::bump(&state.meter().facilitator_errors);
            // Not a 402. The caller's payment may well be fine; telling them to
            // pay again would charge them twice for our outage.
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "error": "facilitator unavailable", "operation": operation, "detail": detail }),
            )
        }

        Decision::Paid {
            payment,
            requirements,
            payer,
        } => {
            forward(
                &state,
                request,
                &method,
                &path,
                query.as_deref(),
                Some((*payment, *requirements, payer)),
            )
            .await
        }
    }
}

/// Relay the request, and if it was paid for and the upstream cooperated, settle.
async fn forward(
    state: &AppState,
    request: Request,
    method: &str,
    path: &str,
    query: Option<&str>,
    settlement: Option<(PaymentPayload, PaymentRequirements, Option<String>)>,
) -> Response {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let body = if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        None
    } else {
        match to_bytes(request.into_body(), MAX_REQUEST_BODY).await {
            Ok(bytes) => Some(bytes.to_vec()),
            Err(_) => {
                // Refuse before spending a call on the upstream. A body that
                // never arrived cannot be priced.
                return json_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({ "error": "request body too large" }),
                );
            }
        }
    };

    let relayed = state
        .upstream()
        .relay(RelayRequest {
            method,
            path,
            query,
            body,
            content_type: content_type.as_deref(),
        })
        .await;

    let upstream_response = match relayed {
        Ok(response) => response,
        Err(error) => {
            Meter::bump(&state.meter().upstream_errors);
            tracing::warn!(%error, path, "upstream failed; nothing will be charged for");
            // 502 is >= 400, so the paywall never settles this. The caller
            // keeps their money and can retry.
            return json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "error": "upstream unreachable", "detail": error.to_string() }),
            );
        }
    };

    let Some((payment, requirements, payer)) = settlement else {
        return upstream_to_response(upstream_response, None);
    };

    if !should_settle(upstream_response.status) {
        Meter::bump(&state.meter().refused);
        tracing::info!(
            status = upstream_response.status,
            path,
            "upstream declined; payment not settled"
        );
        return upstream_to_response(upstream_response, None);
    }

    match state.gate().settle(&payment, &requirements).await {
        Ok(receipt) if receipt.success => {
            Meter::bump(&state.meter().settled);
            tracing::info!(
                transaction = %receipt.transaction,
                payer = payer.as_deref().unwrap_or("unknown"),
                amount = requirements.amount,
                path,
                "settled"
            );
            upstream_to_response(upstream_response, Some(&receipt))
        }

        Ok(receipt) => {
            // The facilitator ran and said no. Do not hand over the data: the
            // authorization did not become a transfer, so serving it would be
            // giving the product away.
            Meter::bump(&state.meter().refused);
            let reason = receipt
                .error_message
                .clone()
                .or_else(|| receipt.error_reason.clone())
                .unwrap_or_else(|| "settlement failed".to_owned());
            tracing::warn!(reason, path, "settlement refused");
            json_response(
                StatusCode::PAYMENT_REQUIRED,
                json!({ "error": "settlement failed", "reason": reason }),
            )
        }

        Err(error) => {
            Meter::bump(&state.meter().facilitator_errors);
            tracing::error!(%error, path, "facilitator unreachable during settlement");
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "error": "facilitator unavailable", "operation": "settle", "detail": error.to_string() }),
            )
        }
    }
}

/// Render a challenge: the base64 quote in a header, and a readable body for
/// anyone who is not an agent.
fn challenge_response(challenge: &PaymentRequired) -> Response {
    let Ok(encoded) = encode_header(challenge) else {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "could not encode the payment challenge" }),
        );
    };

    let body = json!({
        "error": challenge.error,
        "x402Version": challenge.x402_version,
        "resource": challenge.resource,
        "accepts": challenge.accepts,
    });

    let mut response = json_response(StatusCode::PAYMENT_REQUIRED, body);
    if let Ok(value) = encoded.parse() {
        response
            .headers_mut()
            .insert(HEADER_PAYMENT_REQUIRED, value);
        // A quote is specific to the request it was issued for.
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response
}

/// Copy an upstream answer out, optionally attaching the settlement receipt.
fn upstream_to_response(
    upstream: crate::upstream::UpstreamResponse,
    receipt: Option<&SettleResponse>,
) -> Response {
    let status = StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = upstream.content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }

    let mut response = builder
        .body(Body::from(upstream.body))
        .unwrap_or_else(|_| Response::new(Body::empty()));

    if let Some(receipt) = receipt
        && let Ok(encoded) = encode_header(receipt)
        && let Ok(value) = encoded.parse()
    {
        response
            .headers_mut()
            .insert(HEADER_PAYMENT_RESPONSE, value);
    }

    response
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(text))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// The absolute URL a challenge quotes. Behind a proxy the scheme has to come
/// from `X-Forwarded-Proto`, or every quote would claim `http`.
fn resource_url(headers: &HeaderMap, path: &str, query: Option<&str>) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");

    match query {
        Some(query) if !query.is_empty() => format!("{scheme}://{host}{path}?{query}"),
        _ => format!("{scheme}://{host}{path}"),
    }
}

/// The raw payment header, accepting the v1 spelling as well as v2.
fn payment_header(headers: &HeaderMap) -> Option<String> {
    crate::paywall::PAYMENT_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn quotes_the_url_the_caller_actually_used() {
        let h = headers(&[("host", "api.example.com:8080")]);
        assert_eq!(
            resource_url(&h, "/v1/forecast", Some("latitude=52.52")),
            "http://api.example.com:8080/v1/forecast?latitude=52.52"
        );
    }

    #[test]
    fn respects_the_forwarded_scheme() {
        let h = headers(&[("host", "api.example.com"), ("x-forwarded-proto", "https")]);
        assert_eq!(
            resource_url(&h, "/v1/forecast", None),
            "https://api.example.com/v1/forecast"
        );
    }

    #[test]
    fn omits_the_query_marker_when_there_is_no_query() {
        let h = headers(&[("host", "api.example.com")]);
        assert_eq!(
            resource_url(&h, "/v1/x", Some("")),
            "http://api.example.com/v1/x"
        );
    }

    #[test]
    fn reads_both_spellings_of_the_payment_header() {
        assert_eq!(
            payment_header(&headers(&[("payment-signature", "abc")])).as_deref(),
            Some("abc")
        );
        assert_eq!(
            payment_header(&headers(&[("x-payment", "xyz")])).as_deref(),
            Some("xyz")
        );
        assert_eq!(payment_header(&headers(&[])), None);
    }

    #[test]
    fn v2_wins_when_a_client_sends_both() {
        let both = headers(&[("payment-signature", "v2-value"), ("x-payment", "v1-value")]);
        assert_eq!(payment_header(&both).as_deref(), Some("v2-value"));
    }

    #[test]
    fn the_router_builds_without_a_live_facilitator() {
        // Constructing the router must not dial anything: a service has to be
        // able to start while the facilitator is down.
        use crate::chains::MAINNET;
        use crate::facilitator::Facilitator;
        use crate::manifest::{Endpoint, Manifest, Upstream as UpstreamDecl};
        use crate::upstream::Upstream;

        let manifest = Manifest {
            schema: 1,
            name: "demo".to_owned(),
            display_name: "Demo".to_owned(),
            description: "A demo service described at sufficient length.".to_owned(),
            maintainer: None,
            status: "draft".to_owned(),
            network: "eip155:2366".to_owned(),
            pay_to: "0x1234567890123456789012345678901234567890".to_owned(),
            base_url: None,
            upstream: UpstreamDecl {
                name: "Example".to_owned(),
                url: "https://example.com".to_owned(),
                requires_api_key: false,
                terms_url: None,
            },
            endpoints: vec![Endpoint {
                method: "GET".to_owned(),
                path: "/v1/thing".to_owned(),
                summary: "Returns a thing.".to_owned(),
                price_usd: "0.002".to_owned(),
                example_request: None,
                pitfalls: Vec::new(),
            }],
            categories: vec!["data".to_owned()],
            tags: Vec::new(),
            source: None,
        };

        let gate = Gate::new(
            manifest.priced_routes(&MAINNET).unwrap(),
            Facilitator::new("http://127.0.0.1:1"),
            &MAINNET,
        );
        let state = Arc::new(AppState::new(
            gate,
            Upstream::new("https://example.com", "Example", None),
        ));

        let router = build_router(state);
        let _: Router = router;
        let _ = HttpRequest::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty());
    }
}
