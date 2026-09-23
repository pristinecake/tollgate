//! Guards for the `service.yaml` shipped with this repository.
//!
//! The example manifest is documentation, and documentation drifts. These
//! tests make the drift loud: change a price in the YAML without changing the
//! README table, or drop the quotes around `pay_to`, and the suite goes red
//! instead of the deployed service quietly charging something else.
//!
//! The file is also checked against the published Kite schema by CI
//! (`scripts/check-manifest.mjs`). What can be checked from inside Rust is
//! checked here, so a contributor with only a Rust toolchain still gets told.

use std::path::PathBuf;

use tollgate::chains::{MAINNET, TESTNET};
use tollgate::manifest::Manifest;

/// The manifest at the repository root, addressed relative to this crate.
fn shipped_manifest() -> Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("service.yaml");
    Manifest::load(&path).unwrap_or_else(|error| panic!("service.yaml must load: {error}"))
}

#[test]
fn the_shipped_manifest_loads_and_validates() {
    let manifest = shipped_manifest();
    assert_eq!(manifest.schema, 1);
    assert_eq!(manifest.name, "ecb-fx-rates");
    assert_eq!(manifest.network, "eip155:2366");
    assert_eq!(manifest.status, "draft");
    assert_eq!(manifest.endpoints.len(), 3);
}

#[test]
fn the_wallet_survives_yaml_quoting() {
    // `pay_to: 0x2F6a…` without quotes is a YAML integer, and the letters make
    // it a parse error or, worse, a truncated number. The official example
    // manifest calls this out; this test makes sure nobody "tidies" the quotes
    // away.
    let manifest = shipped_manifest();
    assert_eq!(
        manifest.pay_to,
        "0x2F6aB1c0d4E85A39B7c2D9e0F4a1B8c3D6e0A9f2"
    );
    assert_eq!(manifest.pay_to.len(), 42);
}

#[test]
fn the_prices_are_the_ones_the_readme_advertises() {
    // Keep this table and the README table in step. They are the only two
    // places a buyer can find out what a call costs before making it.
    let routes = shipped_manifest().priced_routes(&MAINNET).unwrap();
    let priced: Vec<(&str, &str, &str)> = routes
        .iter()
        .map(|route| {
            (
                route.path.as_str(),
                route.price_usd.as_str(),
                route.amount.as_str(),
            )
        })
        .collect();

    assert_eq!(
        priced,
        vec![
            ("/v1/currencies", "0.0005", "500"),
            ("/v1/latest", "0.001", "1000"),
            ("/v1/timeseries", "0.006", "6000"),
        ]
    );
}

#[test]
fn the_same_manifest_costs_the_printed_price_on_testnet_too() {
    // The testnet token has eighteen decimals instead of six. A decimal price
    // is the same amount of money on both chains; only the integer changes, by
    // a factor of 10^12. A manifest that only priced correctly on one of them
    // would be a trap for anybody testing before going live.
    let manifest = shipped_manifest();
    let mainnet = manifest.priced_routes(&MAINNET).unwrap();
    let testnet = manifest.priced_routes(&TESTNET).unwrap();

    for (main, test) in mainnet.iter().zip(testnet.iter()) {
        assert_eq!(main.price_usd, test.price_usd);
        let scaled: u128 = test.amount.parse().unwrap();
        assert_eq!(
            scaled / 1_000_000_000_000,
            main.amount.parse::<u128>().unwrap(),
            "{}: {} vs {}",
            main.path,
            main.amount,
            test.amount
        );
    }
}

#[test]
fn every_endpoint_tells_the_buyer_how_to_get_it_wrong() {
    // The catalog treats `pitfalls` as the reason a paid call is worth making.
    // An endpoint with no pitfalls is an endpoint nobody has actually used.
    for endpoint in shipped_manifest().endpoints {
        assert!(
            !endpoint.pitfalls.is_empty(),
            "{} has no documented pitfalls",
            endpoint.path
        );
        assert!(
            endpoint.example_request.is_some(),
            "{} ships no example request",
            endpoint.path
        );
    }
}

#[test]
fn the_manifest_declares_itself_honestly() {
    // The published `source` enum has no Rust member, so the honest value is
    // `custom`. If the schema ever grows one, this test should be updated —
    // which is the point of asserting on it.
    assert_eq!(shipped_manifest().source.as_deref(), Some("custom"));
}
