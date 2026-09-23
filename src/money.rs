//! Price arithmetic.
//!
//! Prices live in the manifest as decimal strings (`"0.001"`). Tokens settle
//! in integer units, and the two Kite stablecoins disagree on how many: USDC.e
//! has 6 decimals, pieUSD has 18.
//!
//! This module deliberately never builds a `f64`. `0.001` has no exact binary
//! representation, and once you are multiplying by `10^18` the rounding error
//! lands in the last digits of a signed EIP-3009 authorization — which the
//! facilitator will reject or, worse, accept for a different amount than the
//! one that was quoted. So: parse the literal text, pad it, hand back a string.

use crate::error::PriceError;

/// Turn `"0.001"` into `"1000"` for a 6-decimal token.
///
/// Accepts an optional leading `$` because prices get copy-pasted out of price
/// lists. Rejects anything that is not a plain non-negative decimal.
///
/// # Errors
///
/// [`PriceError::NotADecimal`] when the text has characters a decimal does not,
/// [`PriceError::TooPrecise`] when it needs more precision than the token has
/// (silently truncating would under-charge), and [`PriceError::RoundsToZero`]
/// when the price is positive-looking but collapses to `0` atomic units, which
/// would hand out a paid route for free.
///
/// # Examples
///
/// ```
/// use tollgate::money::to_atomic_units;
///
/// assert_eq!(to_atomic_units("0.001", 6).unwrap(), "1000");
/// assert_eq!(to_atomic_units("$0.25", 6).unwrap(), "250000");
/// assert_eq!(to_atomic_units("1", 18).unwrap(), "1000000000000000000");
/// ```
pub fn to_atomic_units(price: &str, decimals: u8) -> Result<String, PriceError> {
    let input = price.trim();
    let digits_text = input.strip_prefix('$').unwrap_or(input);

    let (whole, fraction) = match digits_text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (digits_text, ""),
    };

    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    // A decimal point commits you to digits on both sides: `"1."` and `".5"`
    // are typos, and `""` / `"1e-3"` are not decimals at all.
    let has_point = digits_text.contains('.');
    if !is_digits(whole) || (has_point && !is_digits(fraction)) {
        return Err(PriceError::NotADecimal {
            input: input.to_owned(),
        });
    }

    let allowed = decimals as usize;
    if fraction.len() > allowed {
        return Err(PriceError::TooPrecise {
            input: input.to_owned(),
            found: fraction.len(),
            allowed,
        });
    }

    let mut scaled = String::with_capacity(whole.len() + allowed);
    scaled.push_str(whole);
    scaled.push_str(fraction);
    scaled.extend(std::iter::repeat_n('0', allowed - fraction.len()));

    let normalized = scaled.trim_start_matches('0');
    if normalized.is_empty() {
        return Err(PriceError::RoundsToZero {
            input: input.to_owned(),
        });
    }

    Ok(normalized.to_owned())
}

/// Human-readable form for the catalog: `"0.001"` + `"USDC.e"` -> `"0.001 USDC.e"`.
pub fn format_price(price: &str, symbol: &str) -> String {
    let bare = price.trim().strip_prefix('$').unwrap_or(price.trim());
    format!("{bare} {symbol}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_fractions_to_the_token_precision() {
        assert_eq!(to_atomic_units("0.001", 6).unwrap(), "1000");
        assert_eq!(to_atomic_units("0.01", 6).unwrap(), "10000");
        assert_eq!(to_atomic_units("0.000001", 6).unwrap(), "1");
        assert_eq!(to_atomic_units("1", 6).unwrap(), "1000000");
        assert_eq!(to_atomic_units("12.5", 6).unwrap(), "12500000");
    }

    #[test]
    fn handles_the_eighteen_decimal_token() {
        assert_eq!(to_atomic_units("1", 18).unwrap(), "1000000000000000000");
        assert_eq!(to_atomic_units("0.000000000000000001", 18).unwrap(), "1");
    }

    #[test]
    fn tolerates_a_currency_prefix_and_surrounding_space() {
        assert_eq!(to_atomic_units("  $0.25 ", 6).unwrap(), "250000");
    }

    #[test]
    fn refuses_more_precision_than_the_token_has() {
        // 0.0000001 USDC.e would have to be truncated to 0 — refuse instead.
        let err = to_atomic_units("0.0000001", 6).unwrap_err();
        assert_eq!(
            err,
            PriceError::TooPrecise {
                input: "0.0000001".to_owned(),
                found: 7,
                allowed: 6,
            }
        );
    }

    #[test]
    fn refuses_zero_and_near_zero() {
        assert_eq!(
            to_atomic_units("0", 6).unwrap_err(),
            PriceError::RoundsToZero {
                input: "0".to_owned()
            }
        );
        assert_eq!(
            to_atomic_units("0.0", 6).unwrap_err(),
            PriceError::RoundsToZero {
                input: "0.0".to_owned()
            }
        );
    }

    #[test]
    fn refuses_things_that_are_not_decimal_numbers() {
        for bad in [
            "", "free", "1e-3", "1.2.3", ".5", "1.", "-1", "0x10", "1 000",
        ] {
            assert!(
                matches!(to_atomic_units(bad, 6), Err(PriceError::NotADecimal { .. })),
                "expected {bad:?} to be rejected, got {:?}",
                to_atomic_units(bad, 6)
            );
        }
    }

    #[test]
    fn zero_decimals_passes_the_integer_through() {
        assert_eq!(to_atomic_units("42", 0).unwrap(), "42");
        assert!(
            to_atomic_units("42.1", 0).is_err(),
            "a 0-decimal token cannot hold a fraction"
        );
    }
}
