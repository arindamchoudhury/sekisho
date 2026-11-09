//! Money as integer micro-USD.

use std::fmt;

/// An amount in micro-USD (1 USD = 1,000,000). Prices are micro-USD per
/// 1,000,000 tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroUsd(pub i64);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error(
        "invalid USD amount {0:?}: use digits with an optional decimal point, such as 250 or 0.0045"
    )]
    Invalid(String),
    #[error("USD amount {0:?} is more precise than 1 micro-USD (6 decimal places)")]
    TooPrecise(String),
    #[error("USD amount {0:?} is too large")]
    TooLarge(String),
}

const MICROS_PER_USD: i64 = 1_000_000;
const DECIMALS: usize = 6;

impl MicroUsd {
    /// Parses a non-negative decimal USD amount such as `"250"` or `"0.0045"`.
    /// Signs, exponents, separators, whitespace and currency symbols are refused.
    pub fn from_usd_str(s: &str) -> Result<Self, MoneyError> {
        let (whole, frac) = match s.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (s, None),
        };
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
        if !digits(whole) || frac.is_some_and(|f| !digits(f)) {
            return Err(MoneyError::Invalid(s.to_owned()));
        }
        let frac = frac.unwrap_or("");
        if frac.len() > DECIMALS {
            return Err(MoneyError::TooPrecise(s.to_owned()));
        }
        let too_large = || MoneyError::TooLarge(s.to_owned());
        let whole: i64 = whole.parse().map_err(|_| too_large())?;
        let frac: i64 = format!("{frac:0<DECIMALS$}")
            .parse()
            .map_err(|_| too_large())?;
        whole
            .checked_mul(MICROS_PER_USD)
            .and_then(|w| w.checked_add(frac))
            .map(MicroUsd)
            .ok_or_else(too_large)
    }
}

impl MicroUsd {
    /// The shortest exact decimal: `4500` is `"0.0045"`, `3_000_000` is `"3"`.
    /// [`MicroUsd::from_usd_str`] reads it back.
    pub fn to_usd_string(self) -> String {
        let per = MICROS_PER_USD.unsigned_abs();
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let (whole, frac) = (abs / per, abs % per);
        if frac == 0 {
            format!("{sign}{whole}")
        } else {
            let frac = format!("{frac:0DECIMALS$}");
            format!("{sign}{whole}.{}", frac.trim_end_matches('0'))
        }
    }
}

/// Prints `$0.004500`; negative amounts print `-$0.004500`.
impl fmt::Display for MicroUsd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let per = MICROS_PER_USD.unsigned_abs();
        write!(f, "{sign}${}.{:0DECIMALS$}", abs / per, abs % per)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_parsing() {
        assert_eq!(MicroUsd::from_usd_str("0.0045").unwrap(), MicroUsd(4500));
        assert_eq!(
            MicroUsd::from_usd_str("250").unwrap(),
            MicroUsd(250_000_000)
        );
        assert!(MicroUsd::from_usd_str("0.0000001").is_err()); // below 1 micro-USD
        assert!(MicroUsd::from_usd_str("-1").is_err());
        assert_eq!(MicroUsd(4500).to_string(), "$0.004500");
    }

    #[test]
    fn usd_parsing_edges() {
        assert_eq!(MicroUsd::from_usd_str("0").unwrap(), MicroUsd(0));
        assert_eq!(MicroUsd::from_usd_str("0.000001").unwrap(), MicroUsd(1));
        assert_eq!(MicroUsd::from_usd_str("1.5").unwrap(), MicroUsd(1_500_000));
        assert_eq!(
            MicroUsd::from_usd_str("9223372036854.775807").unwrap(),
            MicroUsd(i64::MAX)
        );
        for bad in [
            "",
            ".",
            "1.",
            ".5",
            "+1",
            "1e3",
            " 1",
            "1 ",
            "1,000",
            "$1",
            "abc",
            "1.2.3",
            "9223372036854.775808",
            "99999999999999999999",
        ] {
            assert!(MicroUsd::from_usd_str(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn usd_display() {
        assert_eq!(MicroUsd(0).to_string(), "$0.000000");
        assert_eq!(MicroUsd(250_000_000).to_string(), "$250.000000");
        assert_eq!(MicroUsd(-4500).to_string(), "-$0.004500");
        assert_eq!(MicroUsd(i64::MIN).to_string(), "-$9223372036854.775808");
    }

    #[test]
    fn usd_decimal_strings_are_exact_and_short() {
        for (micro, text) in [
            (0, "0"),
            (1, "0.000001"),
            (4_500, "0.0045"),
            (1_250_000, "1.25"),
            (15_000_000, "15"),
            (-4_500, "-0.0045"),
        ] {
            assert_eq!(MicroUsd(micro).to_usd_string(), text);
            assert_eq!(
                MicroUsd::from_usd_str(text.trim_start_matches('-'))
                    .unwrap()
                    .0,
                micro.abs()
            );
        }
    }

    #[test]
    fn usd_ordering() {
        assert!(MicroUsd(1) < MicroUsd(2));
    }
}
