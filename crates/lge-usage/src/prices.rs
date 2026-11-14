//! The price table. Prices are never built in: they change, they differ per model
//! and contract, and a stale built-in number would make every report quietly wrong.

use crate::codex::TokenUsage;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// The configuration file, looked up in the repository root.
pub const CONFIG_FILE: &str = "lge.toml";

/// Price per million tokens, in whatever currency the user wrote them in. The
/// numbers below only show the form; they are nobody's price list.
///
/// ```toml
/// [prices]
/// input_per_mtok = 7.0
/// cached_input_per_mtok = 0.7
/// output_per_mtok = 21.0
/// ```
///
/// There is no reasoning price: reasoning tokens are part of the output tokens
/// and are billed as output.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Prices {
    /// Input tokens that were not read from the cache.
    pub input_per_mtok: f64,
    /// Input tokens that were read from the cache.
    pub cached_input_per_mtok: f64,
    /// Output tokens, reasoning included.
    pub output_per_mtok: f64,
}

impl Prices {
    /// Cost of the given tokens. Input is split into its cached and uncached
    /// part; reasoning is not added, because it is already inside the output.
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let cached = usage.cached_input_tokens.clamp(0, usage.input_tokens.max(0));
        let uncached = usage.input_tokens.max(0) - cached;
        let output = usage.output_tokens.max(0);
        (uncached as f64 * self.input_per_mtok
            + cached as f64 * self.cached_input_per_mtok
            + output as f64 * self.output_per_mtok)
            / 1_000_000.0
    }

    /// A negative or non-finite price would turn the cost columns into nonsense
    /// without any visible sign, so it is refused when the file is loaded.
    fn validate(&self) -> Result<()> {
        let named = [
            ("input_per_mtok", self.input_per_mtok),
            ("cached_input_per_mtok", self.cached_input_per_mtok),
            ("output_per_mtok", self.output_per_mtok),
        ];
        for (name, price) in named {
            if !price.is_finite() || price < 0.0 {
                bail!("prices.{name} must be a non-negative number, found {price}");
            }
        }
        Ok(())
    }
}

/// The part of `lge.toml` this crate reads. Other tables belong to other crates
/// and are ignored here.
#[derive(Deserialize)]
struct ConfigFile {
    prices: Option<Prices>,
}

/// Reads the `[prices]` table of `<root>/lge.toml`.
///
/// `Ok(None)` when the file or the table is absent: no prices configured means no
/// cost columns. A table that is present but incomplete or invalid is an error,
/// not `None`, so a typo cannot silently switch the cost columns off.
pub fn load_prices(root: &Path) -> Result<Option<Prices>> {
    let path = root.join(CONFIG_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let config: ConfigFile = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(prices) = config.prices else {
        return Ok(None);
    };
    prices.validate().with_context(|| format!("in {}", path.display()))?;
    Ok(Some(prices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), config).unwrap();
        dir
    }

    fn error_of(config: &str) -> String {
        format!("{:#}", load_prices(root_with(config).path()).unwrap_err())
    }

    #[test]
    fn no_file_means_no_prices() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_prices(dir.path()).unwrap(), None);
        assert_eq!(load_prices(&dir.path().join("no-such-dir")).unwrap(), None);
    }

    #[test]
    fn file_without_prices_table_means_no_prices() {
        assert_eq!(load_prices(root_with("").path()).unwrap(), None);
        let other = "[checks]\ntest = \"cargo test\"\n\n[router]\nversion = 1\n";
        assert_eq!(load_prices(root_with(other).path()).unwrap(), None);
    }

    #[test]
    fn prices_table_is_read() {
        let config = "[checks]\ntest = \"pytest\"\n\n[prices]\ninput_per_mtok = 7.25\ncached_input_per_mtok = 0.725\noutput_per_mtok = 21.0\n";
        let prices = load_prices(root_with(config).path()).unwrap();
        assert_eq!(
            prices,
            Some(Prices {
                input_per_mtok: 7.25,
                cached_input_per_mtok: 0.725,
                output_per_mtok: 21.0,
            })
        );
    }

    #[test]
    fn whole_numbers_and_zero_are_valid_prices() {
        let config = "[prices]\ninput_per_mtok = 2\ncached_input_per_mtok = 0\noutput_per_mtok = 8\n";
        let prices = load_prices(root_with(config).path()).unwrap().unwrap();
        assert_eq!(
            (
                prices.input_per_mtok,
                prices.cached_input_per_mtok,
                prices.output_per_mtok
            ),
            (2.0, 0.0, 8.0)
        );
    }

    #[test]
    fn incomplete_table_is_an_error_naming_the_key() {
        let err = error_of("[prices]\ninput_per_mtok = 7.25\noutput_per_mtok = 21.0\n");
        assert!(err.contains("cached_input_per_mtok"), "{err}");
        assert!(err.contains(CONFIG_FILE), "{err}");
        assert!(error_of("[prices]\n").contains("input_per_mtok"));
    }

    #[test]
    fn negative_infinite_and_non_numeric_prices_are_errors() {
        let table = |input: &str| {
            format!("[prices]\ninput_per_mtok = {input}\ncached_input_per_mtok = 0.1\noutput_per_mtok = 1.0\n")
        };
        assert!(error_of(&table("-1.0")).contains("prices.input_per_mtok"));
        assert!(error_of(&table("inf")).contains("prices.input_per_mtok"));
        assert!(error_of(&table("nan")).contains("prices.input_per_mtok"));
        assert!(error_of(&table("\"cheap\"")).contains(CONFIG_FILE));
        let negative_output = "[prices]\ninput_per_mtok = 1\ncached_input_per_mtok = 1\noutput_per_mtok = -3\n";
        assert!(error_of(negative_output).contains("prices.output_per_mtok"));
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(error_of("[prices\ninput_per_mtok = ").contains(CONFIG_FILE));
        assert!(error_of("prices = 3\n").contains(CONFIG_FILE));
    }

    #[test]
    fn cost_bills_cached_and_uncached_input_apart_and_reasoning_as_output() {
        let prices = Prices {
            input_per_mtok: 2.0,
            cached_input_per_mtok: 0.5,
            output_per_mtok: 10.0,
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 400_000,
            reasoning_tokens: 150_000,
            output_tokens: 200_000,
        };
        // 600,000 uncached at 2.0 + 400,000 cached at 0.5 + 200,000 output at 10.0.
        // The 150,000 reasoning tokens are inside the output and cost nothing extra.
        assert!((prices.cost(&usage) - 3.4).abs() < 1e-12, "{}", prices.cost(&usage));
        assert_eq!(prices.cost(&TokenUsage::default()), 0.0);
    }

    #[test]
    fn cost_never_bills_more_cached_tokens_than_there_was_input() {
        let prices = Prices {
            input_per_mtok: 2.0,
            cached_input_per_mtok: 1.0,
            output_per_mtok: 0.0,
        };
        let odd = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 5_000_000,
            reasoning_tokens: 0,
            output_tokens: -7,
        };
        assert!((prices.cost(&odd) - 1.0).abs() < 1e-12, "{}", prices.cost(&odd));
    }
}
