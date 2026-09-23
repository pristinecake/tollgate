//! `tollgate` — serve a priced HTTP API behind x402.
//!
//! The output below is the one this repository's own `service.yaml` produces;
//! keep them in step, since a stale example is the kind of documentation a
//! reader trusts and then wastes an hour on.
//!
//! ```text
//! $ tollgate --manifest service.yaml --check
//! service      ECB Reference Rates (ecb-fx-rates)
//! network      eip155:2366 (Kite mainnet, USDC.e)
//! pay_to       0x2F6aB1c0d4E85A39B7c2D9e0F4a1B8c3D6e0A9f2
//! upstream     https://api.frankfurter.app
//!
//! 3 routes priced:
//!   GET  /v1/currencies           0.0005 USDC.e (500 atomic)
//!   GET  /v1/latest               0.001 USDC.e (1000 atomic)
//!   GET  /v1/timeseries           0.006 USDC.e (6000 atomic)
//!
//! $ tollgate --manifest service.yaml
//! ECB Reference Rates listening on 0.0.0.0:8080 -> https://api.frankfurter.app \
//! (USDC.e via eip155:2366, facilitator https://facilitator.pieverse.io/v2)
//! ```
//!
//! `--check` is the one to run in a deploy pipeline: it validates the manifest,
//! prices every endpoint, and exits non-zero on a typo instead of starting a
//! service that will 500 on its first paying call.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tollgate::app::{AppState, serve};
use tollgate::chains;
use tollgate::facilitator::Facilitator;
use tollgate::manifest::Manifest;
use tollgate::money::format_price;
use tollgate::paywall::Gate;
use tollgate::upstream::Upstream;

#[derive(Debug, Parser)]
#[command(
    name = "tollgate",
    version,
    about = "Serve any HTTP API behind x402 payments settled on the Kite chain",
    long_about = "Reads a Kite service manifest, prices each endpoint it declares, and serves them \
                  behind HTTP 402. Payment is verified before the upstream is called and settled \
                  only after it answers, so a failing upstream never costs the caller anything."
)]
struct Cli {
    /// Service manifest to serve.
    #[arg(long, env = "TOLLGATE_MANIFEST", default_value = "service.yaml")]
    manifest: PathBuf,

    /// Facilitator base URL, including its version segment.
    #[arg(long, env = "FACILITATOR_URL", default_value = chains::DEFAULT_FACILITATOR)]
    facilitator: String,

    /// Address to listen on.
    #[arg(long, env = "TOLLGATE_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,

    /// Header to inject on upstream requests, for APIs that want a key.
    #[arg(long, env = "UPSTREAM_AUTH_HEADER", default_value = "Authorization")]
    upstream_auth_header: String,

    /// Value for that header, e.g. "Bearer sk-...". Enables the injection when set.
    #[arg(long, env = "UPSTREAM_AUTH_VALUE")]
    upstream_auth_value: Option<String>,

    /// Validate the manifest and print the price list, then exit.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tollgate=info")),
        )
        .init();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Startup problems belong on stderr in plain language: this is what
            // an operator sees when a manifest is wrong, and a panic message
            // with a backtrace would bury the one useful line.
            eprintln!("tollgate: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let manifest = Manifest::load(&cli.manifest).map_err(|error| error.to_string())?;

    let chain = chains::by_network(&manifest.network).ok_or_else(|| {
        format!(
            "manifest network {:?} is not a Kite chain",
            manifest.network
        )
    })?;

    let routes = manifest
        .priced_routes(chain)
        .map_err(|error| error.to_string())?;

    if cli.check {
        print_prices(&manifest, chain, &routes);
        return Ok(());
    }

    let auth_supplied = cli.upstream_auth_value.is_some();
    let auth = cli
        .upstream_auth_value
        .map(|value| (cli.upstream_auth_header.clone(), value));

    let gate = Gate::new(routes, Facilitator::new(cli.facilitator.clone()), chain);
    let upstream = Upstream::new(
        manifest.upstream.url.clone(),
        manifest.upstream.name.clone(),
        auth,
    );

    let state = Arc::new(AppState::new(gate, upstream));

    println!(
        "{} listening on {} -> {} ({} via {}, facilitator {})",
        manifest.display_name,
        cli.listen,
        manifest.upstream.url,
        chain.stablecoin.symbol,
        chain.network,
        cli.facilitator,
    );
    if manifest.upstream.requires_api_key && !auth_supplied {
        eprintln!(
            "tollgate: {} expects an API key and none was supplied; paid calls will fail upstream \
             until you pass --upstream-auth-value",
            manifest.upstream.name
        );
    }

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .block_on(serve(state, cli.listen))
        .map_err(|error| format!("server stopped: {error}"))
}

fn print_prices(manifest: &Manifest, chain: &chains::Chain, routes: &[tollgate::PricedRoute]) {
    println!("service      {} ({})", manifest.display_name, manifest.name);
    println!(
        "network      {} ({}, {})",
        chain.network, chain.display_name, chain.stablecoin.symbol
    );
    println!("pay_to       {}", manifest.pay_to);
    println!("upstream     {}", manifest.upstream.url);
    println!();
    println!("{} routes priced:", routes.len());
    for route in routes {
        println!(
            "  {:<4} {:<24} {} ({} atomic)",
            route.method,
            route.path,
            format_price(&route.price_usd, chain.stablecoin.symbol),
            route.amount,
        );
    }
}
