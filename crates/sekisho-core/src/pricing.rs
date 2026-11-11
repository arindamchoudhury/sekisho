//! What a request costs: an admin's model prices, the rates they give, and
//! the measures agentgateway reports for each request.

use crate::formats::ApiFormat;
use crate::money::MicroUsd;

/// An admin's prices for a model, in micro-USD per 1,000,000 tokens. The
/// cache prices are optional: [`ModelPrices::rates`] fills them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelPrices {
    pub input: MicroUsd,
    pub output: MicroUsd,
    pub cache_read: Option<MicroUsd>,
    pub cache_write: Option<MicroUsd>,
}

/// Every price a request can need, in micro-USD per 1,000,000 tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rates {
    pub input: MicroUsd,
    pub output: MicroUsd,
    pub cache_read: MicroUsd,
    pub cache_write: MicroUsd,
}

/// A request's token counts. `input` is the whole prompt, cache reads and
/// writes included, as agentgateway's `llm.inputTokens` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl ModelPrices {
    /// The admin's prices, with each missing cache price taken from the
    /// provider's usual ratio to the input price for this API format.
    pub fn rates(&self, format: ApiFormat) -> Rates {
        // (numerator, denominator) of the cache read and cache write prices
        // as fractions of the input price.
        let (read, write) = match format {
            ApiFormat::Anthropic => ((1, 10), (5, 4)),
            ApiFormat::OpenAi => ((1, 2), (1, 1)),
            ApiFormat::Gemini => ((1, 4), (1, 1)),
        };
        Rates {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read.unwrap_or(scaled(self.input, read)),
            cache_write: self.cache_write.unwrap_or(scaled(self.input, write)),
        }
    }
}

/// `price × num / den`, rounded half up.
fn scaled(price: MicroUsd, (num, den): (i128, i128)) -> MicroUsd {
    let value = (i128::from(price.0) * num * 2 + den).div_euclid(den * 2);
    MicroUsd(i64::try_from(value).unwrap_or(i64::MAX))
}

impl Rates {
    /// What `usage` costs at these rates, rounded half up to whole
    /// micro-USD. The uncached prompt is the input less its cache reads and
    /// writes, never below zero.
    pub fn cost(&self, usage: &Usage) -> MicroUsd {
        let uncached = usage
            .input
            .saturating_sub(usage.cache_read)
            .saturating_sub(usage.cache_write);
        let total = [
            (uncached, self.input),
            (usage.cache_read, self.cache_read),
            (usage.cache_write, self.cache_write),
            (usage.output, self.output),
        ]
        .into_iter()
        .fold(0i128, |sum, (tokens, rate)| {
            sum.saturating_add(i128::from(tokens).saturating_mul(i128::from(rate.0)))
        });
        let micro_usd = total.saturating_add(500_000).div_euclid(1_000_000);
        MicroUsd(i64::try_from(micro_usd).unwrap_or(i64::MAX))
    }
}

/// One number agentgateway reports when a request ends, as one rate-limit
/// descriptor each (`measure` entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Measure {
    /// `llm.inputTokens`: the whole prompt.
    Input,
    /// `llm.cachedInputTokens`.
    CacheRead,
    /// `llm.cacheCreationInputTokens`.
    CacheWrite,
    /// `llm.outputTokens`; missing when the provider's usage never arrived.
    Output,
    /// agentgateway's catalog cost, in micro-USD.
    Catalog,
}

impl Measure {
    pub const ALL: [Measure; 5] = [
        Measure::Input,
        Measure::CacheRead,
        Measure::CacheWrite,
        Measure::Output,
        Measure::Catalog,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Measure::Input => "input",
            Measure::CacheRead => "cache_read",
            Measure::CacheWrite => "cache_write",
            Measure::Output => "output",
            Measure::Catalog => "catalog",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::ApiFormat;
    use crate::money::MicroUsd;

    fn rates(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Rates {
        Rates {
            input: MicroUsd(input),
            output: MicroUsd(output),
            cache_read: MicroUsd(cache_read),
            cache_write: MicroUsd(cache_write),
        }
    }

    fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            input,
            cache_read,
            cache_write,
            output,
        }
    }

    #[test]
    fn cost_prices_each_kind_of_token() {
        // 1_000 uncached × 3 + 8_000 × 0.3 + 1_000 × 3.75 + 500 × 15 (USD per million)
        let r = rates(3_000_000, 15_000_000, 300_000, 3_750_000);
        assert_eq!(
            r.cost(&usage(10_000, 8_000, 1_000, 500)),
            MicroUsd(3_000 + 2_400 + 3_750 + 7_500)
        );
    }

    #[test]
    fn cost_rounds_half_up() {
        let r = rates(1, 0, 0, 0);
        assert_eq!(r.cost(&usage(500_000, 0, 0, 0)), MicroUsd(1));
        assert_eq!(r.cost(&usage(499_999, 0, 0, 0)), MicroUsd(0));
    }

    #[test]
    fn cache_parts_larger_than_the_input_leave_no_negative_input() {
        let r = rates(1_000_000, 0, 0, 0);
        assert_eq!(r.cost(&usage(100, 200, 0, 0)), MicroUsd(0));
        assert_eq!(r.cost(&usage(100, 60, 60, 0)), MicroUsd(0));
    }

    #[test]
    fn cost_saturates() {
        let r = rates(i64::MAX, i64::MAX, 0, 0);
        assert_eq!(r.cost(&usage(u64::MAX, 0, 0, u64::MAX)), MicroUsd(i64::MAX));
    }

    #[test]
    fn default_cache_rates_follow_the_format() {
        let prices = ModelPrices {
            input: MicroUsd(3_000_000),
            output: MicroUsd(15_000_000),
            cache_read: None,
            cache_write: None,
        };
        for (format, read, write) in [
            (ApiFormat::Anthropic, 300_000, 3_750_000),
            (ApiFormat::OpenAi, 1_500_000, 3_000_000),
            (ApiFormat::Gemini, 750_000, 3_000_000),
        ] {
            assert_eq!(
                prices.rates(format),
                rates(3_000_000, 15_000_000, read, write),
                "{format:?}"
            );
        }
        let explicit = ModelPrices {
            cache_read: Some(MicroUsd(1)),
            cache_write: Some(MicroUsd(2)),
            ..prices
        };
        for format in ApiFormat::ALL {
            assert_eq!(
                explicit.rates(format),
                rates(3_000_000, 15_000_000, 1, 2),
                "{format:?}"
            );
        }
    }

    #[test]
    fn default_cache_rates_round_half_up() {
        // 0.1 × 5 = 0.5 → 1; 1.25 × 2 = 2.5 → 3
        let prices = ModelPrices {
            input: MicroUsd(5),
            output: MicroUsd(0),
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(prices.rates(ApiFormat::Anthropic).cache_read, MicroUsd(1));
        let prices = ModelPrices {
            input: MicroUsd(2),
            ..prices
        };
        assert_eq!(prices.rates(ApiFormat::Anthropic).cache_write, MicroUsd(3));
    }

    #[test]
    fn measures_round_trip() {
        let names: Vec<&str> = Measure::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(
            names,
            ["input", "cache_read", "cache_write", "output", "catalog"]
        );
        for measure in Measure::ALL {
            assert_eq!(Measure::parse(measure.as_str()), Some(measure));
        }
        assert_eq!(Measure::parse("Input"), None);
    }
}
