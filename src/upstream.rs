//! Relaying a paid request to the API being wrapped.
//!
//! Path mapping follows the official templates: this service exposes `GET
//! /v1/forecast`, and the `/v1` prefix is stripped before the request is joined
//! to the upstream origin. So an upstream of `https://api.open-meteo.com/v1`
//! receives `https://api.open-meteo.com/v1/forecast`. Putting the version
//! segment in the upstream URL (rather than hardcoding it here) is what lets
//! one wrapper front APIs that version their paths differently.
//!
//! Two things never make the trip upstream: the caller's `PAYMENT-SIGNATURE`
//! (it is for this server, not for them) and hop-by-hop headers, which are
//! meaningless once the connection is not the same one.

use std::time::Duration;

use crate::error::UpstreamError;

/// How long the upstream gets before we give up and refuse to charge.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Headers that describe a single hop and must not be forwarded.
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// The API this service fronts.
#[derive(Debug, Clone)]
pub struct Upstream {
    base_url: String,
    display_name: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
}

/// One request to relay.
#[derive(Debug, Clone)]
pub struct RelayRequest<'a> {
    pub method: &'a str,
    /// The path as this service exposes it, `/v1/...`.
    pub path: &'a str,
    /// Raw query string, without the leading `?`.
    pub query: Option<&'a str>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<&'a str>,
}

/// The upstream's answer, buffered so the paywall can inspect the status
/// before deciding whether to settle.
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl Upstream {
    /// Build a relay target.
    ///
    /// `auth` is an optional `(header, value)` pair injected on the way out —
    /// how a wrapper fronts an API that wants a key the buyer does not have.
    pub fn new(
        base_url: impl Into<String>,
        display_name: impl Into<String>,
        auth: Option<(String, String)>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .expect("reqwest client should build with the rustls feature enabled");
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            display_name: display_name.into(),
            client,
            auth,
        }
    }

    /// The upstream origin plus any version prefix, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The upstream's name, for logs and `/healthz`.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Where `path` lands upstream, `/v1` stripped.
    ///
    /// ```
    /// use tollgate::upstream::target_url;
    ///
    /// assert_eq!(
    ///     target_url("https://api.open-meteo.com/v1", "/v1/forecast", Some("latitude=52.52")),
    ///     "https://api.open-meteo.com/v1/forecast?latitude=52.52"
    /// );
    /// assert_eq!(
    ///     target_url("https://api.example.com", "/v1/a/b", None),
    ///     "https://api.example.com/a/b"
    /// );
    /// ```
    pub fn target(&self, path: &str, query: Option<&str>) -> String {
        target_url(&self.base_url, path, query)
    }

    /// Send the request upstream and buffer the answer.
    ///
    /// # Errors
    ///
    /// [`UpstreamError::Transport`] when the upstream cannot be reached, and
    /// [`UpstreamError::Status`] never — a 500 from upstream is a *successful*
    /// relay that the caller must see, because it is the reason they will not
    /// be charged.
    pub async fn relay(
        &self,
        request: RelayRequest<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let url = self.target(request.path, request.query);
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            UpstreamError::Status {
                url: url.clone(),
                status: 400,
            }
        })?;

        let mut outgoing = self.client.request(method, &url);

        if let Some((header, value)) = &self.auth {
            outgoing = outgoing.header(header.as_str(), value.as_str());
        }
        if let Some(content_type) = request.content_type {
            outgoing = outgoing.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if let Some(body) = request.body {
            outgoing = outgoing.body(body);
        }

        let response = outgoing
            .send()
            .await
            .map_err(|source| UpstreamError::Transport {
                url: url.clone(),
                source,
            })?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        let body = response
            .bytes()
            .await
            .map_err(|source| UpstreamError::Transport {
                url: url.clone(),
                source,
            })?
            .to_vec();

        Ok(UpstreamResponse {
            status,
            content_type,
            body,
        })
    }
}

/// Join an upstream origin to a service path, stripping the `/v1` segment.
pub fn target_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let stripped = path.strip_prefix("/v1").unwrap_or(path);
    match query.filter(|q| !q.is_empty()) {
        Some(query) => format!("{base}{stripped}?{query}"),
        None => format!("{base}{stripped}"),
    }
}

/// Whether a header should be forwarded to the upstream.
pub fn is_forwardable_header(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    if HOP_BY_HOP.contains(&lowered.as_str()) {
        return false;
    }
    // The payment is addressed to this server. Relaying it would leak a signed
    // authorization to a third party, who could present it to the facilitator.
    lowered != crate::protocol::HEADER_PAYMENT_SIGNATURE.to_ascii_lowercase()
        && lowered != crate::protocol::HEADER_X_PAYMENT.to_ascii_lowercase()
        && lowered != crate::protocol::HEADER_PAYMENT_REQUIRED.to_ascii_lowercase()
        && lowered != crate::protocol::HEADER_PAYMENT_RESPONSE.to_ascii_lowercase()
        && lowered != "host"
        && lowered != "content-length"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_proxy_prefix_and_keeps_the_version_on_the_upstream_side() {
        assert_eq!(
            target_url("https://api.open-meteo.com/v1", "/v1/forecast", None),
            "https://api.open-meteo.com/v1/forecast"
        );
    }

    #[test]
    fn keeps_nested_segments() {
        assert_eq!(
            target_url("https://api.example.com", "/v1/a/b/c", None),
            "https://api.example.com/a/b/c"
        );
    }

    #[test]
    fn tolerates_a_trailing_slash_on_the_origin() {
        assert_eq!(
            target_url("https://api.example.com/", "/v1/x", None),
            "https://api.example.com/x"
        );
    }

    #[test]
    fn appends_the_query_string_verbatim() {
        assert_eq!(
            target_url("https://api.example.com", "/v1/x", Some("a=1&b=2")),
            "https://api.example.com/x?a=1&b=2"
        );
        assert_eq!(
            target_url("https://api.example.com", "/v1/x", Some("")),
            "https://api.example.com/x",
            "an empty query must not leave a dangling ?"
        );
    }

    #[test]
    fn paths_without_the_prefix_are_left_alone() {
        assert_eq!(
            target_url("https://api.example.com", "/health", None),
            "https://api.example.com/health"
        );
    }

    #[test]
    fn the_payment_headers_stay_home() {
        assert!(!is_forwardable_header("PAYMENT-SIGNATURE"));
        assert!(!is_forwardable_header("payment-signature"));
        assert!(!is_forwardable_header("X-PAYMENT"));
        assert!(!is_forwardable_header("Host"));
        assert!(!is_forwardable_header("content-length"));
        assert!(!is_forwardable_header("Transfer-Encoding"));
        assert!(is_forwardable_header("Accept"));
        assert!(is_forwardable_header("User-Agent"));
        assert!(is_forwardable_header("Authorization"));
    }
}
