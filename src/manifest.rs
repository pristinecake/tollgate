//! Reading a Kite `service.yaml` and turning it into priced routes.
//!
//! The manifest is the same file the official
//! [`kite-x402-services`](https://github.com/gokite-ai/kite-x402-services)
//! catalog uses to describe a service: a name, the upstream it wraps, the
//! wallet that gets paid, and one entry per endpoint with a price. Serving that
//! file directly — instead of inventing a second config format — means a
//! service published to the catalog and a service running here are the same
//! artifact, and there is no way for the price a buyer reads to drift from the
//! price the server charges.
//!
//! Validation follows the published schema, including the parts that only bite
//! at runtime: prices are parsed here, at boot, rather than on the first paid
//! request.
//!
//! ```
//! use tollgate::{chains, manifest::Manifest};
//!
//! let yaml = r#"
//! schema: 1
//! name: demo
//! display_name: Demo
//! description: A demo service that is long enough to pass validation.
//! maintainer: { github: someone }
//! status: draft
//! network: eip155:2366
//! pay_to: "0x1234567890123456789012345678901234567890"
//! upstream: { name: Example, url: https://example.com }
//! categories: [data]
//! endpoints:
//!   - method: GET
//!     path: /v1/thing
//!     summary: Returns a thing from the example upstream.
//!     price_usd: "0.002"
//! "#;
//!
//! let manifest = Manifest::from_yaml_str("demo.yaml", yaml)?;
//! let routes = manifest.priced_routes(&chains::MAINNET)?;
//! assert_eq!(routes[0].amount, "2000");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::path::Path;

use serde::Deserialize;

use crate::chains::Chain;
use crate::error::ManifestError;
use crate::money::to_atomic_units;
use crate::protocol::{PaymentRequirements, SCHEME_EXACT};

/// The only manifest schema revision this build understands.
pub const SUPPORTED_SCHEMA: i64 = 1;

/// Longest authorization lifetime handed to a client, in seconds.
const AUTHORIZATION_WINDOW_SECONDS: u64 = 60;

/// Where the payments go.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Maintainer {
    pub github: String,
    #[serde(default)]
    pub contact: Option<String>,
}

/// The API being wrapped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Upstream {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub requires_api_key: bool,
    #[serde(default)]
    pub terms_url: Option<String>,
}

/// One endpoint, and what it costs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Endpoint {
    /// Upper-case HTTP verb: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`.
    pub method: String,
    /// Path this service exposes, `/v1/...`.
    pub path: String,
    /// One line a buyer reads before paying.
    pub summary: String,
    /// Decimal USD, quoted in YAML so `0.010` keeps its trailing zero.
    pub price_usd: String,
    /// A request known to succeed. Kept for the catalog; not used at runtime.
    #[serde(default)]
    pub example_request: Option<serde_json::Value>,
    /// Ways to lose money by guessing wrong. Kept for the catalog.
    #[serde(default)]
    pub pitfalls: Vec<String>,
}

/// A parsed `service.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub schema: i64,
    pub name: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub maintainer: Option<Maintainer>,
    /// `draft`, `testnet`, or `live`.
    pub status: String,
    /// CAIP-2 identifier of the settlement network.
    pub network: String,
    /// Kite wallet that receives payment, `0x`-prefixed.
    pub pay_to: String,
    /// Public origin this service is deployed at. Required once deployed.
    #[serde(default)]
    pub base_url: Option<String>,
    pub upstream: Upstream,
    pub endpoints: Vec<Endpoint>,
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Which template the wrapper started from.
    #[serde(default)]
    pub source: Option<String>,
}

/// An endpoint with its price already converted to atomic units, ready to be
/// dropped into a challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricedRoute {
    /// Upper-case HTTP verb.
    pub method: String,
    pub path: String,
    pub summary: String,
    /// The price as written in the manifest, e.g. `"0.002"`.
    pub price_usd: String,
    /// The same price in the token's smallest unit, e.g. `"2000"`.
    pub amount: String,
    /// The `accepts` entry quoted to clients for this route.
    pub requirements: PaymentRequirements,
}

impl Manifest {
    /// Read and check a manifest from disk.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Read`] if the file cannot be read,
    /// [`ManifestError::Yaml`] if it is not valid YAML, plus everything
    /// [`Manifest::validate`] rejects.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let manifest: Self =
            serde_yaml_ng::from_str(&text).map_err(|source| ManifestError::Yaml {
                path: path.display().to_string(),
                source,
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parse a manifest that is already in memory.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Yaml`] if it is not valid YAML, plus everything
    /// [`Manifest::validate`] rejects.
    pub fn from_yaml_str(name: impl Into<String>, text: &str) -> Result<Self, ManifestError> {
        let name = name.into();
        let manifest: Self = serde_yaml_ng::from_str(text)
            .map_err(|source| ManifestError::Yaml { path: name, source })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Check the rules the schema expresses, plus the ones it cannot.
    ///
    /// Prices are converted here rather than per request so a typo fails at
    /// boot. A service that starts up and then 500s on its first paying
    /// customer is worse than one that refuses to start.
    ///
    /// # Errors
    ///
    /// The matching [`ManifestError`] variant for whichever rule is broken.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(ManifestError::UnsupportedSchema { found: self.schema });
        }

        let chain = crate::chains::by_network(&self.network).ok_or_else(|| {
            ManifestError::UnknownNetwork {
                network: self.network.clone(),
            }
        })?;

        if !is_address(&self.pay_to) {
            return Err(ManifestError::BadPayTo {
                pay_to: self.pay_to.clone(),
            });
        }

        if !(self.upstream.url.starts_with("https://") || self.upstream.url.starts_with("http://"))
        {
            return Err(ManifestError::BadUpstreamUrl {
                url: self.upstream.url.clone(),
            });
        }

        if self.endpoints.is_empty() {
            return Err(ManifestError::NoEndpoints {
                path: self.name.clone(),
            });
        }

        let mut seen: Vec<(String, String)> = Vec::with_capacity(self.endpoints.len());
        for endpoint in &self.endpoints {
            let method = endpoint.method.to_ascii_uppercase();
            let key = (method.clone(), endpoint.path.clone());
            if seen.contains(&key) {
                return Err(ManifestError::DuplicateEndpoint {
                    method,
                    path: endpoint.path.clone(),
                });
            }
            seen.push(key);

            to_atomic_units(&endpoint.price_usd, chain.stablecoin.decimals).map_err(|source| {
                ManifestError::BadPrice {
                    method,
                    path: endpoint.path.clone(),
                    price: endpoint.price_usd.clone(),
                    source,
                }
            })?;
        }

        Ok(())
    }

    /// Build the runtime route table for `chain`.
    ///
    /// # Errors
    ///
    /// [`ManifestError::BadPrice`] when a price cannot be expressed in the
    /// chain's token.
    pub fn priced_routes(&self, chain: &Chain) -> Result<Vec<PricedRoute>, ManifestError> {
        self.endpoints
            .iter()
            .map(|endpoint| {
                let method = endpoint.method.to_ascii_uppercase();
                let amount = to_atomic_units(&endpoint.price_usd, chain.stablecoin.decimals)
                    .map_err(|source| ManifestError::BadPrice {
                        method: method.clone(),
                        path: endpoint.path.clone(),
                        price: endpoint.price_usd.clone(),
                        source,
                    })?;

                Ok(PricedRoute {
                    method,
                    path: endpoint.path.clone(),
                    summary: endpoint.summary.clone(),
                    price_usd: endpoint.price_usd.clone(),
                    amount: amount.clone(),
                    requirements: PaymentRequirements {
                        scheme: SCHEME_EXACT.to_owned(),
                        network: chain.network.to_owned(),
                        asset: chain.stablecoin.address.to_owned(),
                        amount,
                        pay_to: self.pay_to.clone(),
                        max_timeout_seconds: AUTHORIZATION_WINDOW_SECONDS,
                        // The facilitator re-derives the EIP-712 domain from
                        // these two strings. Wrong name, no settlement.
                        extra: serde_json::Map::from_iter([
                            (
                                "name".to_owned(),
                                serde_json::Value::String(chain.stablecoin.domain_name.to_owned()),
                            ),
                            (
                                "version".to_owned(),
                                serde_json::Value::String(
                                    chain.stablecoin.domain_version.to_owned(),
                                ),
                            ),
                        ]),
                    },
                })
            })
            .collect()
    }
}

/// Structural check for `0x` + 40 hex digits. Not an EIP-55 checksum check —
/// the facilitator is the authority on validity, and rejecting a
/// non-checksummed-but-correct address would only annoy people.
fn is_address(candidate: &str) -> bool {
    let Some(hex) = candidate
        .strip_prefix("0x")
        .or_else(|| candidate.strip_prefix("0X"))
    else {
        return false;
    };
    hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::{MAINNET, TESTNET};

    const GOOD: &str = r#"
schema: 1
name: demo
display_name: Demo Service
description: A demo service described at sufficient length for validation.
maintainer:
  github: someone
status: draft
network: eip155:2366
pay_to: "0x1234567890123456789012345678901234567890"
upstream:
  name: Example
  url: https://example.com
endpoints:
  - method: GET
    path: /v1/thing
    summary: Returns a thing from the example upstream.
    price_usd: "0.002"
categories: [data]
"#;

    #[test]
    fn parses_and_prices_against_mainnet() {
        let manifest = Manifest::from_yaml_str("demo.yaml", GOOD).unwrap();
        let routes = manifest.priced_routes(&MAINNET).unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method, "GET");
        assert_eq!(
            routes[0].amount, "2000",
            "0.002 USDC.e is 2000 atomic units"
        );
        assert_eq!(routes[0].requirements.network, "eip155:2366");
        assert_eq!(
            routes[0]
                .requirements
                .extra
                .get("name")
                .and_then(|v| v.as_str()),
            Some("Bridged USDC (Kite AI)"),
            "the EIP-712 domain name rides along in extra"
        );
    }

    #[test]
    fn the_same_manifest_prices_differently_on_testnet() {
        // pieUSD has 18 decimals; the atomic amount must follow the chain.
        let yaml = GOOD.replace("eip155:2366", "eip155:2368");
        let manifest = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap();
        let routes = manifest.priced_routes(&TESTNET).unwrap();
        assert_eq!(routes[0].amount, "2000000000000000");
    }

    #[test]
    fn rejects_a_price_the_token_cannot_express() {
        // 0.0000001 is finer than USDC.e's 6 decimals. Validation runs at load
        // time, so the service refuses to start rather than truncating the
        // price to zero and serving the route for free.
        let yaml = GOOD.replace("\"0.002\"", "\"0.0000001\"");
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(
            matches!(
                err,
                ManifestError::BadPrice {
                    ref price,
                    source: crate::error::PriceError::TooPrecise { found: 7, allowed: 6, .. },
                    ..
                } if price == "0.0000001"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_a_bad_wallet_address() {
        // A wallet typed without quotes parses as a YAML integer upstream;
        // either way it must not reach a challenge.
        let yaml = GOOD.replace("\"0x1234", "\"0xZZZZ");
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(matches!(err, ManifestError::BadPayTo { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_an_unknown_network() {
        let yaml = GOOD.replace("eip155:2366", "eip155:8453");
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(
            matches!(err, ManifestError::UnknownNetwork { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_a_duplicated_endpoint() {
        // Two entries for the same verb and path would leave the price
        // ambiguous — and the route table is looked up by exactly that pair.
        let yaml = GOOD.replace(
            "categories: [data]",
            "  - method: GET\n    path: /v1/thing\n    summary: The same path again, twice over.\n    price_usd: \"0.001\"\ncategories: [data]",
        );
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(
            matches!(err, ManifestError::DuplicateEndpoint { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn allows_the_same_path_under_a_different_verb() {
        // GET and POST on one path are two endpoints with two prices.
        let yaml = GOOD.replace(
            "categories: [data]",
            "  - method: POST\n    path: /v1/thing\n    summary: Same path, different verb, different price.\n    price_usd: \"0.005\"\ncategories: [data]",
        );
        let manifest = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap();
        let routes = manifest.priced_routes(&MAINNET).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[1].method, "POST");
        assert_eq!(routes[1].amount, "5000");
    }

    #[test]
    fn rejects_an_empty_endpoint_list() {
        let yaml = GOOD.replace(
            "endpoints:\n  - method: GET\n    path: /v1/thing\n    summary: Returns a thing from the example upstream.\n    price_usd: \"0.002\"\n",
            "endpoints: []\n",
        );
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(
            matches!(err, ManifestError::NoEndpoints { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_a_future_schema_revision() {
        let yaml = GOOD.replace("schema: 1", "schema: 2");
        let err = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap_err();
        assert!(
            matches!(err, ManifestError::UnsupportedSchema { found: 2 }),
            "got {err:?}"
        );
    }

    #[test]
    fn endpoint_keys_are_normalised_for_lookup() {
        // Manifests are written by hand; `get` in lowercase is a common slip
        // and the official catalog accepts it, so routing must too.
        let yaml = GOOD.replace("method: GET", "method: get");
        let manifest = Manifest::from_yaml_str("demo.yaml", &yaml).unwrap();
        let routes = manifest.priced_routes(&MAINNET).unwrap();
        assert_eq!(routes[0].method, "GET");
    }

    #[test]
    fn address_check_accepts_both_prefix_cases_and_rejects_short_input() {
        assert!(is_address("0x1234567890123456789012345678901234567890"));
        assert!(is_address("0X1234567890123456789012345678901234567890"));
        assert!(!is_address("0x1234"));
        assert!(!is_address("1234567890123456789012345678901234567890"));
        assert!(!is_address("0x12345678901234567890123456789012345678zz"));
    }
}
