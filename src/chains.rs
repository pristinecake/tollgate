//! The two Kite chains this server can settle on, and the stablecoin each one pays in.
//!
//! Both settle through EIP-3009: the payer signs a `transferWithAuthorization`
//! and the facilitator re-derives the EIP-712 domain hash to recover the
//! signer. That domain hash is built from the token's `name` and `version`, so
//! a challenge that carries the right contract address but the wrong name will
//! fail verification forever, with a facilitator error that never mentions the
//! name. The constants below are the whole fix for that class of bug.

/// A stablecoin the paywall can quote prices in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stablecoin {
    /// ERC-20 contract address on its chain.
    pub address: &'static str,
    /// Ticker as it appears in price lists.
    pub symbol: &'static str,
    /// Smallest-unit exponent: `6` means one token is `10^6` atomic units.
    pub decimals: u8,
    /// EIP-712 domain `name`, hashed by the facilitator.
    pub domain_name: &'static str,
    /// EIP-712 domain `version`.
    pub domain_version: &'static str,
}

/// A Kite network plus the token that settles on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chain {
    /// Short key used in logs and the manifest (`mainnet` / `testnet`).
    pub key: &'static str,
    /// CAIP-2 network identifier, as it appears in `PaymentRequirements.network`.
    pub network: &'static str,
    pub display_name: &'static str,
    pub rpc_url: &'static str,
    pub explorer_url: &'static str,
    pub stablecoin: Stablecoin,
}

/// Kite mainnet. Real money: USDC.e, 6 decimals.
pub const MAINNET: Chain = Chain {
    key: "mainnet",
    network: "eip155:2366",
    display_name: "Kite mainnet",
    rpc_url: "https://rpc.gokite.ai",
    explorer_url: "https://kitescan.ai",
    stablecoin: Stablecoin {
        address: "0x7aB6f3ed87C42eF0aDb67Ed95090f8bF5240149e",
        symbol: "USDC.e",
        decimals: 6,
        domain_name: "Bridged USDC (Kite AI)",
        domain_version: "2",
    },
};

/// Kite testnet. Free-to-mint pieUSD, 18 decimals — the exponent difference is
/// the reason prices never go through a float.
pub const TESTNET: Chain = Chain {
    key: "testnet",
    network: "eip155:2368",
    display_name: "Kite testnet",
    rpc_url: "https://rpc-testnet.gokite.ai",
    explorer_url: "https://testnet.kitescan.ai",
    stablecoin: Stablecoin {
        address: "0x38129cf4CE5E183eFF248F42A7D345Bb1B47621A",
        symbol: "pieUSD",
        decimals: 18,
        domain_name: "pieUSD",
        domain_version: "1",
    },
};

/// Kite's hosted facilitator. The `/v2` segment belongs to the base URL.
pub const DEFAULT_FACILITATOR: &str = "https://facilitator.pieverse.io/v2";

/// Look a chain up by its CAIP-2 identifier.
pub fn by_network(network: &str) -> Option<&'static Chain> {
    match network {
        "eip155:2366" => Some(&MAINNET),
        "eip155:2368" => Some(&TESTNET),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_both_kite_networks() {
        assert_eq!(
            by_network("eip155:2366").unwrap().stablecoin.symbol,
            "USDC.e"
        );
        assert_eq!(
            by_network("eip155:2368").unwrap().stablecoin.symbol,
            "pieUSD"
        );
        assert!(
            by_network("eip155:8453").is_none(),
            "Base is not a Kite chain"
        );
        assert!(by_network("").is_none());
    }

    #[test]
    fn mainnet_token_matches_the_deployed_contract() {
        // Guards against a copy-paste edit: USDC.e is 6 decimals and its
        // EIP-712 domain name is not the plain ticker.
        assert_eq!(MAINNET.stablecoin.decimals, 6);
        assert_eq!(MAINNET.stablecoin.domain_name, "Bridged USDC (Kite AI)");
        assert!(MAINNET.stablecoin.address.starts_with("0x"));
        assert_eq!(MAINNET.stablecoin.address.len(), 42);
    }

    #[test]
    fn every_chain_agrees_with_its_network_constant() {
        for chain in [&MAINNET, &TESTNET] {
            assert_eq!(by_network(chain.network).unwrap().network, chain.network);
            assert!(chain.rpc_url.starts_with("https://"));
        }
    }
}
