//! What a request costs: an admin's model prices, the rates they give, and
//! the measures agentgateway reports for each request.

use crate::formats::{ProviderKind, is_claude};
use crate::money::MicroUsd;

/// Where a model's missing cache prices come from: its maker's usual ratio
/// to the input price, or for an open-weight host the input price itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CacheDefaults {
    /// Read 0.1, write 1.25 times the input price.
    Anthropic,
    /// Read 0.5, write 1.0 times the input price.
    OpenAi,
    /// Read 0.25, write 1.0 times the input price.
    Gemini,
    /// Read and write at the input price: an open-weight host's discounts,
    /// if any, are its own, and a budget must not undercount.
    InputPrice,
}

impl CacheDefaults {
    /// By the model, not the API its provider is reached through: Claude
    /// on Bedrock or Azure keeps Anthropic's ratios ([`is_claude`]), and
    /// every other Bedrock and Azure model takes OpenAI's.
    pub fn for_model(kind: ProviderKind, model: &str) -> CacheDefaults {
        match kind {
            _ if is_claude(kind, model) => CacheDefaults::Anthropic,
            ProviderKind::Gemini => CacheDefaults::Gemini,
            ProviderKind::OpenAiCompatible => CacheDefaults::InputPrice,
            ProviderKind::Anthropic
            | ProviderKind::OpenAi
            | ProviderKind::Bedrock
            | ProviderKind::Azure => CacheDefaults::OpenAi,
        }
    }

    /// (numerator, denominator) of the cache read and cache write prices
    /// as fractions of the input price.
    fn ratios(self) -> ((i128, i128), (i128, i128)) {
        match self {
            CacheDefaults::Anthropic => ((1, 10), (5, 4)),
            CacheDefaults::OpenAi => ((1, 2), (1, 1)),
            CacheDefaults::Gemini => ((1, 4), (1, 1)),
            CacheDefaults::InputPrice => ((1, 1), (1, 1)),
        }
    }
}

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
    /// The prices, with each missing cache price taken from `defaults`,
    /// rounded half up.
    pub fn rates(&self, defaults: CacheDefaults) -> Rates {
        let (read, write) = defaults.ratios();
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
    /// A descriptor without a cost: agentgateway sends its `tokenize` count
    /// of the prompt on the request-side call. All Sekisho hears of a stream
    /// that ends early, since agentgateway v1.7.0 sends no response-side
    /// call for one. Not a cost on a response-side call.
    Prompt,
}

impl Measure {
    pub const ALL: [Measure; 6] = [
        Measure::Input,
        Measure::CacheRead,
        Measure::CacheWrite,
        Measure::Output,
        Measure::Catalog,
        Measure::Prompt,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Measure::Input => "input",
            Measure::CacheRead => "cache_read",
            Measure::CacheWrite => "cache_write",
            Measure::Output => "output",
            Measure::Catalog => "catalog",
            Measure::Prompt => "prompt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::ProviderKind;
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
    fn cache_defaults_follow_the_model() {
        use CacheDefaults as C;
        use ProviderKind as K;
        for (kind, model, want) in [
            (K::Anthropic, "claude-x", C::Anthropic),
            (
                K::Bedrock,
                "us.anthropic.claude-sonnet-4-5-v1:0",
                C::Anthropic,
            ),
            (K::Bedrock, "ANTHROPIC.CLAUDE-x", C::Anthropic),
            (K::Bedrock, "amazon.nova-pro", C::OpenAi),
            (K::Bedrock, "openai.gpt-oss-120b-1:0", C::OpenAi),
            (K::Azure, "claude-sonnet", C::Anthropic),
            (K::Azure, "Claude-x", C::Anthropic),
            (K::Azure, "gpt-5", C::OpenAi),
            (K::OpenAi, "gpt-5", C::OpenAi),
            (K::OpenAi, "claude-x", C::OpenAi),
            (K::Gemini, "gemini-2.5-pro", C::Gemini),
            (K::OpenAiCompatible, "claude-lookalike", C::InputPrice),
            (K::OpenAiCompatible, "qwen3", C::InputPrice),
        ] {
            assert_eq!(
                CacheDefaults::for_model(kind, model),
                want,
                "{kind:?} {model}"
            );
        }
    }

    #[test]
    fn default_cache_rates_follow_the_defaults() {
        let prices = ModelPrices {
            input: MicroUsd(3_000_000),
            output: MicroUsd(15_000_000),
            cache_read: None,
            cache_write: None,
        };
        let all = [
            (CacheDefaults::Anthropic, 300_000, 3_750_000),
            (CacheDefaults::OpenAi, 1_500_000, 3_000_000),
            (CacheDefaults::Gemini, 750_000, 3_000_000),
            (CacheDefaults::InputPrice, 3_000_000, 3_000_000),
        ];
        for (defaults, read, write) in all {
            assert_eq!(
                prices.rates(defaults),
                rates(3_000_000, 15_000_000, read, write),
                "{defaults:?}"
            );
        }
        let explicit = ModelPrices {
            cache_read: Some(MicroUsd(1)),
            cache_write: Some(MicroUsd(2)),
            ..prices
        };
        for (defaults, _, _) in all {
            assert_eq!(
                explicit.rates(defaults),
                rates(3_000_000, 15_000_000, 1, 2),
                "{defaults:?}"
            );
        }
        // One given, the other defaulted.
        let read_only = ModelPrices {
            cache_read: Some(MicroUsd(7)),
            ..prices
        };
        assert_eq!(
            read_only.rates(CacheDefaults::InputPrice),
            rates(3_000_000, 15_000_000, 7, 3_000_000)
        );
    }

    #[test]
    fn default_cache_rates_round_half_up() {
        // 0.1 × 5 = 0.5 → 1; 1.25 × 2 = 2.5 → 3; 0.25 × 2 = 0.5 → 1
        let prices = ModelPrices {
            input: MicroUsd(5),
            output: MicroUsd(0),
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(
            prices.rates(CacheDefaults::Anthropic).cache_read,
            MicroUsd(1)
        );
        let prices = ModelPrices {
            input: MicroUsd(2),
            ..prices
        };
        assert_eq!(
            prices.rates(CacheDefaults::Anthropic).cache_write,
            MicroUsd(3)
        );
        assert_eq!(prices.rates(CacheDefaults::Gemini).cache_read, MicroUsd(1));
    }

    #[test]
    fn measures_round_trip() {
        let names: Vec<&str> = Measure::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(
            names,
            [
                "input",
                "cache_read",
                "cache_write",
                "output",
                "catalog",
                "prompt"
            ]
        );
        for measure in Measure::ALL {
            assert_eq!(Measure::parse(measure.as_str()), Some(measure));
        }
        assert_eq!(Measure::parse("Input"), None);
    }
}
