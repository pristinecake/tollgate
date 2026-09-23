//! tollgate — a manifest-driven x402 paywall for any HTTP API, in Rust.
//!
//! Point it at a [`manifest`] describing an upstream API and a price per
//! endpoint, and it serves those endpoints behind HTTP 402. Unpaid callers get
//! a challenge; paid callers get the upstream response *and* a settlement
//! receipt, and only after the upstream has actually answered — a request that
//! fails upstream is never charged.
//!
//! # Layout
//!
//! - [`protocol`] — the x402 v2 wire types. Replaces the SDK that does not
//!   exist for Rust.
//! - [`facilitator`] — HTTP client for `/supported`, `/verify`, `/settle`.
//! - [`manifest`] — parses and prices a Kite `service.yaml`.
//! - [`paywall`] — the decision logic, with no web-framework types in it.
//! - [`app`] — the axum router that wires the two together.
//! - [`chains`], [`money`] — Kite networks and exact price arithmetic.
//! - [`upstream`] — relays the paid request onward.
//!
//! # Example
//!
//! ```no_run
//! use tollgate::{chains, manifest::Manifest};
//!
//! let manifest = Manifest::load("service.yaml")?;
//! let chain = chains::by_network(&manifest.network).expect("known Kite chain");
//! let routes = manifest.priced_routes(chain)?;
//! println!("{} routes, first costs {} atomic units", routes.len(), routes[0].amount);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod app;
pub mod chains;
pub mod error;
pub mod facilitator;
pub mod manifest;
pub mod money;
pub mod paywall;
pub mod protocol;
pub mod upstream;

pub use error::{FacilitatorError, ManifestError, PriceError, ProtocolError, UpstreamError};
pub use facilitator::Facilitator;
pub use manifest::{Manifest, PricedRoute};
pub use paywall::{Decision, Gate};
pub use upstream::Upstream;
