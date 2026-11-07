//! Tokens a round used and what they cost at the prices the user entered.
//! No price is built in: providers change them, so a model without a price
//! shows its tokens only.
use serde::{Deserialize, Serialize};

/// Tokens reported by the provider. `input` includes the cached tokens, as
/// the transport normalizes every provider to that convention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    /// Input tokens read from the provider's prompt cache.
    pub cache_read: Option<u64>,
    /// Input tokens written to the provider's prompt cache.
    pub cache_write: Option<u64>,
}

impl Usage {
    /// The sum of two rounds, keeping a count unknown only when both are.
    pub fn plus(self, other: Self) -> Self {
        fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            }
        }
        Self {
            input: add(self.input, other.input),
            output: add(self.output, other.output),
            cache_read: add(self.cache_read, other.cache_read),
            cache_write: add(self.cache_write, other.cache_write),
        }
    }
}

/// Prices per million tokens, in the currency the user chose to enter.
/// A cache price left empty is charged as plain input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
}

impl Price {
    /// What `usage` costs, or `None` when the provider did not report the
    /// input or output count.
    pub fn cost(&self, usage: &Usage) -> Option<f64> {
        let input = usage.input?;
        let output = usage.output?;
        let read = usage.cache_read.unwrap_or(0).min(input);
        let write = usage.cache_write.unwrap_or(0).min(input - read);
        let plain = input - read - write;
        let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        Some(
            per(plain, self.input_per_mtok)
                + per(
                    read,
                    self.cache_read_per_mtok.unwrap_or(self.input_per_mtok),
                )
                + per(
                    write,
                    self.cache_write_per_mtok.unwrap_or(self.input_per_mtok),
                )
                + per(output, self.output_per_mtok),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_charges_cached_input_at_its_own_price() {
        let price = Price {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: Some(0.3),
            cache_write_per_mtok: None,
        };
        let usage = Usage {
            input: Some(1_000_000),
            output: Some(100_000),
            cache_read: Some(500_000),
            cache_write: Some(100_000),
        };
        // 400k plain × 3 + 500k × 0.3 + 100k written × 3 + 100k out × 15.
        let cost = price.cost(&usage).unwrap();
        assert!((cost - (1.2 + 0.15 + 0.3 + 1.5)).abs() < 1e-9, "{cost}");
        assert_eq!(price.cost(&Usage::default()), None);
    }

    #[test]
    fn usages_add_up_and_unknown_stays_unknown() {
        let a = Usage {
            input: Some(10),
            output: None,
            ..Usage::default()
        };
        let b = Usage {
            input: Some(5),
            output: None,
            cache_read: Some(1),
            ..Usage::default()
        };
        let sum = a.plus(b);
        assert_eq!(sum.input, Some(15));
        assert_eq!(sum.output, None);
        assert_eq!(sum.cache_read, Some(1));
    }
}
