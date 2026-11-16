//! Rough per-million-token prices for cost estimates. Shown as estimates only.

/// (input, output, cache_read, cache_write) USD per 1M tokens.
pub fn per_million(model: Option<&str>) -> (f64, f64, f64, f64) {
    let m = model.unwrap_or("").to_ascii_lowercase();
    if m.contains("opus-5-5") {
        (4.0, 20.0, 0.20, 5.0)
    } else if m.contains("fable-5-1") {
        (10.0, 50.0, 0.25, 12.5)
    } else if m.contains("fable") || m.contains("mythos") {
        (10.0, 50.0, 1.0, 12.5)
    } else if ["opus-4-5", "opus-4-6", "opus-4-7", "opus-4-8"]
        .iter()
        .any(|s| m.contains(s))
    {
        (5.0, 25.0, 0.50, 6.25)
    } else if m.contains("opus") {
        (15.0, 75.0, 1.50, 18.75)
    } else if m.contains("haiku-5-5") {
        (0.1, 0.5, 0.01, 0.125)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.10, 1.25)
    } else if m.contains("gpt-6.1-sol") {
        (2.0, 10.0, 0.10, 2.5)
    } else if m.contains("gpt-6-sol") {
        (2.0, 10.0, 0.20, 2.5)
    } else if m.contains("gpt-6-astra") {
        (10.0, 50.0, 1.0, 12.5)
    } else if m.contains("gpt-6-luna") {
        (0.1, 0.5, 0.01, 0.125)
    } else if m.contains("gpt-5.6-terra") {
        (2.0, 12.0, 0.20, 2.5)
    } else if m.contains("gpt-5.6-sol") {
        (4.0, 20.0, 0.40, 5.0)
    } else if m.contains("gpt-5.6-luna") {
        (0.2, 1.2, 0.02, 0.25)
    } else if m.contains("gpt-5.4") {
        (2.5, 15.0, 0.25, 0.0)
    } else if m.contains("gpt-5.5") {
        (5.0, 30.0, 0.50, 0.0)
    } else if m.contains("gpt-5.2") || m.contains("gpt-5.3") {
        (1.75, 14.0, 0.175, 0.0)
    } else if m.contains("gpt-5") || m.contains("codex") {
        (1.25, 10.0, 0.125, 0.0)
    } else {
        (3.0, 15.0, 0.30, 3.75)
    }
}

pub fn estimate(
    model: Option<&str>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
) -> f64 {
    let (i, o, cr, cw) = per_million(model);
    (input as f64 * i + output as f64 * o + cache_read as f64 * cr + cache_write as f64 * cw)
        / 1_000_000.0
}

/// Prompt tokens past which OpenAI bills a call at its long-context rates
/// (the 272k threshold on its price list).
const LONG_CONTEXT_PROMPT_TOKENS: i64 = 272_000;

/// Estimate one call, where prompt size determines supported long-context rates.
/// `cache_write_1h` is a subset of `cache_write`, not additional tokens.
pub fn estimate_call(
    model: Option<&str>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cache_write_1h: i64,
) -> f64 {
    let m = model.unwrap_or("").to_ascii_lowercase();
    let (mut i, mut o, mut cr, mut cw) = per_million(model);
    let long =
        input.saturating_add(cache_read).saturating_add(cache_write) > LONG_CONTEXT_PROMPT_TOKENS;
    if long
        && ["gpt-6", "gpt-5.6", "gpt-5.4", "gpt-5.5"]
            .iter()
            .any(|s| m.contains(s))
    {
        i *= 2.0;
        o *= 1.5;
        cr *= 2.0;
        cw *= 2.0;
    }
    let hour = cache_write_1h.max(0).min(cache_write.max(0));
    (input.max(0) as f64 * i
        + output.max(0) as f64 * o
        + cache_read.max(0) as f64 * cr
        + (cache_write.max(0) - hour) as f64 * cw
        + hour as f64 * i * 2.0)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_models_do_not_fall_back_to_legacy_prices() {
        assert_eq!(per_million(Some("gpt-6.1-sol")), (2.0, 10.0, 0.1, 2.5));
        assert_eq!(per_million(Some("claude-opus-5-5")), (4.0, 20.0, 0.2, 5.0));
        assert_eq!(
            per_million(Some("claude-fable-5-1")),
            (10.0, 50.0, 0.25, 12.5)
        );
        assert!(
            (estimate_call(Some("claude-opus-5-5"), 0, 0, 0, 1000, 1000) - 0.008).abs() < 1e-12
        );
        assert!(
            (estimate_call(Some("gpt-6.1-sol"), 1000, 1000, 300000, 0, 0) - 0.079).abs() < 1e-12
        );
    }
}
