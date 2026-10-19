use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Output throughput for one submitted query, including its tools and waiting time.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Performance {
    pub output_tokens: u64,
    pub elapsed_seconds: f64,
    pub estimated: bool,
    #[serde(default)]
    pub session_output_tokens: Option<u64>,
}

impl Performance {
    pub fn new(output_tokens: u64, elapsed: Duration, estimated: bool) -> Self {
        Self {
            output_tokens,
            elapsed_seconds: elapsed.as_secs_f64().max(0.001),
            estimated,
            session_output_tokens: None,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}{:.0} tps",
            if self.estimated { "~" } else { "" },
            self.output_tokens as f64 / self.elapsed_seconds.max(0.001)
        )
    }

    pub fn tooltip(&self) -> String {
        format!(
            "{} output tokens in {:.1}s. Average over this query, including tools and waiting.{}",
            self.output_tokens,
            self.elapsed_seconds,
            if self.estimated {
                " Token count estimated from output text (about 4 characters per token)."
            } else {
                " Token count reported by the model provider."
            }
        )
    }
}

pub fn estimate_tokens(characters: usize) -> u64 {
    characters.div_ceil(4) as u64
}

/// Codex reports session totals when resuming, so only the delta belongs to this query.
pub fn codex_performance(
    total: u64,
    baseline: Option<u64>,
    characters: usize,
    elapsed: Duration,
) -> Performance {
    let delta = baseline.and_then(|previous| total.checked_sub(previous));
    let mut performance = Performance::new(
        delta.unwrap_or_else(|| estimate_tokens(characters)),
        elapsed,
        delta.is_none(),
    );
    performance.session_output_tokens = Some(total);
    performance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_codex_rates_use_query_delta_and_handle_missing_or_reset_baselines() {
        let stats = codex_performance(21882, Some(21872), 13, Duration::from_secs_f64(4.784367292));
        assert_eq!(stats.output_tokens, 10);
        assert_eq!(stats.label(), "2 tps");
        assert!(!stats.estimated);
        assert_eq!(stats.session_output_tokens, Some(21882));
        let fresh = codex_performance(70, Some(0), 13, Duration::from_secs(5));
        assert_eq!(fresh.label(), "14 tps");
        for baseline in [None, Some(30000)] {
            let stats = codex_performance(21882, baseline, 13, Duration::from_secs(2));
            assert_eq!(stats.output_tokens, 4);
            assert!(stats.estimated);
            assert_eq!(stats.session_output_tokens, Some(21882));
        }
    }
}
