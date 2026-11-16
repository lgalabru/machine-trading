//! Token usage per turn: what our built-in adapters measure, and how it
//! reaches the host.
//!
//! Each harness counts differently, so each adapter keeps a meter that turns
//! its harness's reports into one turn's spend:
//!
//! - **Claude** ends every turn with a `result` whose `usage` is that turn's
//!   alone, while `total_cost_usd` and `modelUsage` keep running for the life
//!   of the process. The turn's cost is how much the running total grew.
//!   A message steered into a turn ends in a `result` of its own, so a
//!   prompt can take several, and the meter adds them up.
//! - **Codex** reports `thread/tokenUsage/updated` with the thread's running
//!   `total` and the newest response's `last`. A turn is what `total` grew
//!   by. The first report after a (re)start, or a `total` that went down,
//!   counts `last` only, so a resumed thread's history is never billed twice.
//!
//! The adapter puts the turn's spend in the ACP prompt reply: `usage` with the
//! counts (`inputTokens` excludes cache reads and writes, `outputTokens`
//! includes reasoning) and `_meta.axel` with the model and the cost when the
//! harness reported one. The host reads `usage` only next to `_meta.axel`:
//! ACP leaves what it counts open.
//!
//! A turn can run for hours, so the spend also comes in as it happens: at
//! most every [`PROGRESS_EVERY`], an `_axel/usage` notification carries the
//! tokens spent since the last one, in the same shape. Claude's are its
//! `assistant` messages' usage (each API call once, by message id); Codex's
//! are its reports. The reply then holds only what no progress report
//! carried, plus the turn's cost, so nothing is counted twice.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::pricing;
use serde_json::{Value, json};

/// One of a plan's usage windows, as harnesses report them. (Axel's
/// `axel_store::LimitWindow`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitWindow {
    /// The rolling five hours (Claude's and Codex's session limit).
    FiveHour,
    Week,
    /// Free and lower plans' monthly allowance.
    Month,
}

impl LimitWindow {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitWindow::FiveHour => "five_hour",
            LimitWindow::Week => "week",
            LimitWindow::Month => "month",
        }
    }

    /// The window lasting `mins` minutes.
    pub fn from_minutes(mins: u64) -> LimitWindow {
        if mins >= 28 * 24 * 60 {
            LimitWindow::Month
        } else if mins >= 7 * 24 * 60 {
            LimitWindow::Week
        } else {
            LimitWindow::FiveHour
        }
    }
}

impl std::str::FromStr for LimitWindow {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "five_hour" => Ok(LimitWindow::FiveHour),
            "week" => Ok(LimitWindow::Week),
            "month" => Ok(LimitWindow::Month),
            other => Err(format!("unknown limit window {other}")),
        }
    }
}

/// How often a running turn reports what it has spent so far.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// One turn's spend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnUsage {
    /// Input not read from or written to the prompt cache.
    pub input: u64,
    /// Output, reasoning included.
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The part of `output` spent reasoning.
    pub reasoning: u64,
    /// What the harness says the turn cost, when it says.
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
}

impl TurnUsage {
    pub fn is_empty(&self) -> bool {
        self.total_tokens() == 0 && self.cost_usd.is_none_or(|c| c == 0.0)
    }

    /// Every token counted, saturating: the counts come from the harness,
    /// and a bogus one must not wrap.
    fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// `self` less `other`'s token counts, each floored at zero; the cost
    /// and model stay.
    pub fn less_tokens(mut self, other: &TurnUsage) -> TurnUsage {
        self.input = self.input.saturating_sub(other.input);
        self.output = self.output.saturating_sub(other.output);
        self.cache_read = self.cache_read.saturating_sub(other.cache_read);
        self.cache_write = self.cache_write.saturating_sub(other.cache_write);
        self.reasoning = self
            .reasoning
            .saturating_sub(other.reasoning)
            .min(self.output);
        debug_assert!(
            self.reasoning <= self.output,
            "reasoning is part of the output"
        );
        self
    }

    fn add(&mut self, other: TurnUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        if other.model.is_some() {
            self.model = other.model;
        }
    }

    /// `reply` (a prompt reply, `{"stopReason": …}`) with this usage in it.
    pub fn into_reply(self, mut reply: Value) -> Value {
        if self.is_empty() {
            return reply;
        }
        reply["usage"] = json!({
            "totalTokens": self.total_tokens(),
            "inputTokens": self.input,
            "outputTokens": self.output,
            "thoughtTokens": self.reasoning,
            "cachedReadTokens": self.cache_read,
            "cachedWriteTokens": self.cache_write,
        });
        let mut axel = json!({});
        if let Some(c) = self.cost_usd {
            axel["costUsd"] = json!(c);
        }
        if let Some(m) = &self.model {
            axel["model"] = json!(m);
        }
        reply["_meta"] = json!({ "axel": axel });
        reply
    }

    /// The usage in a prompt reply, as [`Self::into_reply`] wrote it.
    /// `None` without `_meta.axel`: ACP leaves what `usage` counts open
    /// (per turn or per session, cache in input or not), so only usage in
    /// Axel's terms is read.
    pub fn from_reply(reply: &Value) -> Option<TurnUsage> {
        let u = reply.get("usage").filter(|u| u.is_object())?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let axel = Some(reply.pointer("/_meta/axel").filter(|a| a.is_object())?);
        let usage = TurnUsage {
            input: n("inputTokens"),
            output: n("outputTokens"),
            cache_read: n("cachedReadTokens"),
            cache_write: n("cachedWriteTokens"),
            reasoning: n("thoughtTokens"),
            cost_usd: axel.and_then(|a| a.get("costUsd")).and_then(Value::as_f64),
            model: axel
                .and_then(|a| a.get("model"))
                .and_then(Value::as_str)
                .map(String::from),
        };
        (!usage.is_empty()).then_some(usage)
    }
}

/// Claude's turns, from its `assistant` messages and `result`s.
#[derive(Debug, Default)]
pub struct ClaudeMeter {
    /// `total_cost_usd` at the last result: the process's running total.
    cost_seen: f64,
    /// A resumed process may report historical spend in its first result.
    resumed_cost_pending: bool,
    /// Each model's running `costUSD`, to tell which one did the turn.
    model_cost_seen: std::collections::HashMap<String, f64>,
    pending: TurnUsage,
    /// Each API call's usage this prompt, by message id: Claude repeats a
    /// message once per content block.
    calls: HashMap<String, TurnUsage>,
    /// Spent this prompt and not yet reported as progress.
    live: TurnUsage,
    /// Reported as progress this prompt.
    reported: TurnUsage,
    last_progress: Option<Instant>,
    /// Tokens in context after the newest main-agent response.
    context_used: Option<u64>,
    context_size: Option<u64>,
}

impl ClaudeMeter {
    /// Resume without attributing the native conversation's old cost to this process.
    pub fn resumed() -> Self {
        Self {
            resumed_cost_pending: true,
            ..Default::default()
        }
    }

    /// An `assistant` message: what its API call spent (subagents' too),
    /// and how full the main context is now. Synthetic messages spend
    /// nothing; subagents' say nothing about the main context.
    pub fn on_assistant(&mut self, message: &Value) {
        let msg = message.get("message").unwrap_or(&Value::Null);
        if msg.get("model").and_then(Value::as_str) == Some("<synthetic>") {
            return;
        }
        let Some(u) = msg.get("usage") else { return };
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        if let Some(id) = msg.get("id").and_then(Value::as_str) {
            let call = TurnUsage {
                input: n("input_tokens"),
                output: n("output_tokens"),
                cache_read: n("cache_read_input_tokens"),
                cache_write: n("cache_creation_input_tokens"),
                model: msg.get("model").and_then(Value::as_str).map(String::from),
                ..Default::default()
            };
            // The same call again: only what grew since.
            let seen = self.calls.entry(id.to_string()).or_default();
            let grew = call.clone().less_tokens(seen);
            seen.input = seen.input.max(call.input);
            seen.output = seen.output.max(call.output);
            seen.cache_read = seen.cache_read.max(call.cache_read);
            seen.cache_write = seen.cache_write.max(call.cache_write);
            if !grew.is_empty() {
                self.live.add(grew);
            }
        }
        if message
            .get("parent_tool_use_id")
            .is_some_and(|p| !p.is_null())
        {
            return;
        }
        let used = n("input_tokens")
            .saturating_add(n("cache_read_input_tokens"))
            .saturating_add(n("cache_creation_input_tokens"))
            .saturating_add(n("output_tokens"));
        if used > 0 {
            self.context_used = Some(used);
        }
    }

    /// A `result`: the turn's (or steered segment's) usage, added to the
    /// prompt's.
    pub fn on_result(&mut self, result: &Value) {
        let resumed = std::mem::take(&mut self.resumed_cost_pending);
        let u = result.get("usage").unwrap_or(&Value::Null);
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let mut turn = TurnUsage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            reasoning: u
                .pointer("/output_tokens_details/thinking_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            ..Default::default()
        };
        if let Some(total) = result.get("total_cost_usd").and_then(Value::as_f64) {
            // A running total that went down is a new process: all of it is new.
            let delta = if total + 1e-12 >= self.cost_seen {
                total - self.cost_seen
            } else {
                total
            };
            self.cost_seen = total;
            turn.cost_usd = Some(delta.max(0.0));
        }
        // The model whose running cost grew most did the turn.
        let mut best: Option<(String, f64)> = None;
        for (model, m) in result
            .get("modelUsage")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let cost = m.get("costUSD").and_then(Value::as_f64).unwrap_or(0.0);
            let seen = self
                .model_cost_seen
                .insert(model.clone(), cost)
                .unwrap_or(0.0);
            let grew = if cost >= seen { cost - seen } else { cost };
            if best.as_ref().is_none_or(|(_, b)| grew > *b) {
                best = Some((model.clone(), grew));
            }
        }
        if let Some((model, _)) = &best
            && let Some(size) = result
                .pointer(&format!(
                    "/modelUsage/{}/contextWindow",
                    model.replace('~', "~0").replace('/', "~1")
                ))
                .and_then(Value::as_u64)
        {
            self.context_size = Some(size);
        }
        turn.model = best.map(|(m, _)| m);
        if resumed {
            // Seed the cumulative baseline, but price only calls observed here.
            // A result replayed on resume with no new usage therefore costs zero.
            let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            let estimate = |u: &TurnUsage| {
                pricing::estimate_call(
                    u.model.as_deref(),
                    count(u.input),
                    count(u.output),
                    count(u.cache_read),
                    count(u.cache_write),
                    0,
                )
            };
            turn.cost_usd = Some(if self.calls.is_empty() {
                estimate(&turn)
            } else {
                self.calls.values().map(estimate).sum()
            });
            if let Some(call) = self.calls.values().max_by_key(|u| u.total_tokens()) {
                turn.model.clone_from(&call.model);
            }
        }
        self.pending.add(turn);
    }

    /// What was spent since the last progress report, when there is some
    /// and the last report is at least [`PROGRESS_EVERY`] old.
    pub fn progress(&mut self, now: Instant) -> Option<TurnUsage> {
        if self.live.is_empty()
            || self
                .last_progress
                .is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY)
        {
            return None;
        }
        self.last_progress = Some(now);
        let p = std::mem::take(&mut self.live);
        self.reported.add(p.clone());
        Some(p)
    }

    /// The prompt's usage less what progress reports carried, and a fresh
    /// start for the next.
    pub fn take(&mut self) -> TurnUsage {
        let reported = std::mem::take(&mut self.reported);
        self.live = TurnUsage::default();
        self.calls.clear();
        std::mem::take(&mut self.pending).less_tokens(&reported)
    }

    /// `(used, size)` of the context window, once both are known.
    pub fn context(&self) -> Option<(u64, u64)> {
        Some((self.context_used?, self.context_size?))
    }
}

/// One of Codex's token breakdowns.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Breakdown {
    /// Input, cached included.
    input: u64,
    cached: u64,
    cache_write: u64,
    /// Output, reasoning included.
    output: u64,
    reasoning: u64,
}

impl Breakdown {
    fn parse(v: Option<&Value>) -> Option<Breakdown> {
        let v = v.filter(|v| v.is_object())?;
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(Breakdown {
            input: n("inputTokens"),
            cached: n("cachedInputTokens"),
            cache_write: n("cacheWriteInputTokens"),
            output: n("outputTokens"),
            reasoning: n("reasoningOutputTokens"),
        })
    }

    /// `self - before`, or `None` when any counter went down.
    fn since(self, before: Breakdown) -> Option<Breakdown> {
        Some(Breakdown {
            input: self.input.checked_sub(before.input)?,
            cached: self.cached.checked_sub(before.cached)?,
            cache_write: self.cache_write.checked_sub(before.cache_write)?,
            output: self.output.checked_sub(before.output)?,
            reasoning: self.reasoning.checked_sub(before.reasoning)?,
        })
    }

    /// Saturating: the counts come from Codex, and a bogus one must not wrap.
    fn plus(self, o: Breakdown) -> Breakdown {
        Breakdown {
            input: self.input.saturating_add(o.input),
            cached: self.cached.saturating_add(o.cached),
            cache_write: self.cache_write.saturating_add(o.cache_write),
            output: self.output.saturating_add(o.output),
            reasoning: self.reasoning.saturating_add(o.reasoning),
        }
    }
}

/// Codex's turns, from `thread/tokenUsage/updated`.
#[derive(Debug, Default)]
pub struct CodexMeter {
    baseline: Option<Breakdown>,
    pending: Breakdown,
    /// The part of `pending` reported as progress.
    reported: Breakdown,
    last_progress: Option<Instant>,
    pub model: Option<String>,
}

impl CodexMeter {
    /// A meter for a thread running `model`.
    pub fn new(model: Option<String>) -> Self {
        CodexMeter {
            model,
            ..Default::default()
        }
    }

    /// One `tokenUsage` report.
    pub fn observe(&mut self, token_usage: &Value) {
        let Some(total) = Breakdown::parse(token_usage.get("total")) else {
            return;
        };
        let last = Breakdown::parse(token_usage.get("last")).unwrap_or_default();
        let delta = match self.baseline {
            Some(b) => total.since(b).unwrap_or(last),
            None => last,
        };
        self.baseline = Some(total);
        self.pending = self.pending.plus(delta);
    }

    /// What was spent since the last progress report, when there is some
    /// and the last report is at least [`PROGRESS_EVERY`] old.
    pub fn progress(&mut self, now: Instant) -> Option<TurnUsage> {
        if self
            .last_progress
            .is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY)
        {
            return None;
        }
        let p = self.usage(self.pending.since(self.reported).unwrap_or_default());
        if p.is_empty() {
            return None;
        }
        self.last_progress = Some(now);
        self.reported = self.pending;
        Some(p)
    }

    /// Spend since the last `take` that no progress report carried.
    /// Reports that arrive after a turn ends count toward the next.
    pub fn take(&mut self) -> TurnUsage {
        let b = std::mem::take(&mut self.pending)
            .since(std::mem::take(&mut self.reported))
            .unwrap_or_default();
        self.usage(b)
    }

    fn usage(&self, b: Breakdown) -> TurnUsage {
        TurnUsage {
            input: b
                .input
                .saturating_sub(b.cached.saturating_add(b.cache_write)),
            output: b.output,
            cache_read: b.cached,
            cache_write: b.cache_write,
            reasoning: b.reasoning.min(b.output),
            cost_usd: None,
            model: self.model.clone(),
        }
    }
}

/// One plan window's use, as an adapter tells the host (`_axel/limits`).
#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub window: LimitWindow,
    /// 0–100.
    pub used_pct: f64,
    /// When it starts over (Unix seconds).
    pub resets_at: Option<i64>,
}

/// The `_axel/limits` notification for `limits`.
pub fn limits_params(limits: &[Limit]) -> Value {
    json!({ "windows": limits.iter().map(|l| json!({ "window": l.window.as_str(), "usedPercent": l.used_pct, "resetsAt": l.resets_at })).collect::<Vec<_>>() })
}

/// The windows in an `_axel/limits` notification.
pub fn limits_from_params(params: &Value) -> Vec<Limit> {
    params
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| {
            Some(Limit {
                window: w.get("window")?.as_str()?.parse().ok()?,
                used_pct: w.get("usedPercent")?.as_f64()?,
                resets_at: w.get("resetsAt").and_then(Value::as_i64),
            })
        })
        .collect()
}

/// Claude's `rate_limit_event`: `unifiedWindows` holds each window's
/// `utilization` (0–1) and `resetsAt`.
pub fn claude_limits(event: &Value) -> Vec<Limit> {
    let Some(windows) = event
        .pointer("/rate_limit_info/unifiedWindows")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    windows
        .iter()
        .filter_map(|(name, w)| {
            let window = match name.as_str() {
                "five_hour" => LimitWindow::FiveHour,
                "seven_day" => LimitWindow::Week,
                _ => return None, // model-scoped windows and others: not the plan's
            };
            Some(Limit {
                window,
                used_pct: (w.get("utilization")?.as_f64()? * 100.0).clamp(0.0, 100.0),
                resets_at: w.get("resetsAt").and_then(Value::as_i64),
            })
        })
        .collect()
}

/// Codex's rate limits (`account/rateLimits/read`'s reply, or the
/// `account/rateLimits/updated` notification): `primary` and `secondary`
/// windows with `usedPercent` (0–100), and on business plans (where those
/// are null) a monthly credit allowance, `individualLimit`, with
/// `remainingPercent`. Codex's own limits are under
/// `rateLimitsByLimitId.codex` when it lists several; a snapshot for another
/// limit (one model's) isn't the plan's.
pub fn codex_limits(params: &Value) -> Vec<Limit> {
    let Some(snap) = params
        .pointer("/rateLimitsByLimitId/codex")
        .or_else(|| params.get("rateLimits"))
        .filter(|s| s.is_object())
    else {
        return Vec::new();
    };
    if snap
        .get("limitId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != "codex")
    {
        return Vec::new();
    }
    let monthly = matches!(
        snap.get("planType").and_then(Value::as_str),
        Some("free" | "go")
    );
    let positions = [
        (
            "primary",
            if monthly {
                LimitWindow::Month
            } else {
                LimitWindow::FiveHour
            },
        ),
        ("secondary", LimitWindow::Week),
    ];
    let mut limits: Vec<Limit> = positions
        .into_iter()
        .filter_map(|(at, fallback)| {
            let w = snap.get(at).filter(|w| w.is_object())?;
            Some(Limit {
                window: w
                    .get("windowDurationMins")
                    .and_then(Value::as_u64)
                    .map_or(fallback, LimitWindow::from_minutes),
                used_pct: w.get("usedPercent")?.as_f64()?.clamp(0.0, 100.0),
                resets_at: w.get("resetsAt").and_then(Value::as_i64),
            })
        })
        .collect();
    if let Some(credit) = snap.get("individualLimit").filter(|c| c.is_object())
        && let Some(remaining) = credit.get("remainingPercent").and_then(Value::as_f64)
        && !limits.iter().any(|l| l.window == LimitWindow::Month)
    {
        limits.push(Limit {
            window: LimitWindow::Month,
            used_pct: (100.0 - remaining).clamp(0.0, 100.0),
            resets_at: credit.get("resetsAt").and_then(Value::as_i64),
        });
    }
    limits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, input: u64, output: u64, cache_read: u64) -> Value {
        json!({ "type": "assistant", "parent_tool_use_id": null, "message": { "id": id, "model": "claude-opus-5-5",
            "usage": { "input_tokens": input, "output_tokens": output, "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": 0 } } })
    }

    #[test]
    fn claude_reports_spend_as_it_goes_and_the_reply_holds_the_rest() {
        let t0 = Instant::now();
        let mut m = ClaudeMeter::default();
        m.on_assistant(&assistant("m1", 10, 5, 1000));
        m.on_assistant(&assistant("m1", 10, 5, 1000));
        let p = m.progress(t0).expect("first report");
        assert_eq!(
            (p.input, p.output, p.cache_read),
            (10, 5, 1000),
            "a call repeated per content block counts once"
        );
        m.on_assistant(&assistant("m2", 20, 7, 2000));
        assert_eq!(
            m.progress(t0 + Duration::from_secs(1)),
            None,
            "not more often than every 15s"
        );
        let p = m.progress(t0 + PROGRESS_EVERY).expect("second report");
        assert_eq!((p.input, p.output, p.cache_read), (20, 7, 2000));
        m.on_result(&json!({ "type": "result", "total_cost_usd": 0.5,
            "usage": { "input_tokens": 31, "output_tokens": 12, "cache_read_input_tokens": 3000, "cache_creation_input_tokens": 0 } }));
        let rest = m.take();
        assert_eq!(
            (rest.input, rest.output, rest.cache_read),
            (1, 0, 0),
            "only what no report carried"
        );
        assert_eq!(rest.cost_usd, Some(0.5), "the turn's cost, whole");
        m.on_assistant(&assistant("m1", 10, 5, 1000));
        assert!(
            m.progress(t0 + PROGRESS_EVERY * 2).is_some(),
            "a new prompt starts over"
        );
    }

    #[test]
    fn codex_reports_spend_as_it_goes_and_the_reply_holds_the_rest() {
        let t0 = Instant::now();
        let mut m = CodexMeter::new(Some("gpt-5".into()));
        let report = |total: u64| json!({ "total": { "inputTokens": total, "outputTokens": total / 10 }, "last": { "inputTokens": total, "outputTokens": total / 10 } });
        m.observe(&report(100));
        assert_eq!(m.progress(t0).map(|p| p.input), Some(100));
        m.observe(&report(250));
        let rest = m.take();
        assert_eq!(
            (rest.input, rest.output),
            (150, 15),
            "the reply: what came after the last report"
        );
        assert_eq!(m.progress(t0 + PROGRESS_EVERY), None, "nothing new");
    }

    /// The two results a real `claude -p` printed for two prompts in one
    /// process (trimmed): `usage` per turn, cost and `modelUsage` running.
    fn claude_results() -> [Value; 2] {
        [
            json!({ "type": "result", "total_cost_usd": 0.00364334,
                "usage": { "input_tokens": 2, "cache_creation_input_tokens": 17588, "cache_read_input_tokens": 11854, "output_tokens": 14, "output_tokens_details": { "thinking_tokens": 0 } },
                "modelUsage": { "claude-haiku-5-5": { "inputTokens": 2, "outputTokens": 14, "costUSD": 0.00364334, "contextWindow": 1000000 } } }),
            json!({ "type": "result", "total_cost_usd": 0.00911025,
                "usage": { "input_tokens": 2, "cache_creation_input_tokens": 26545, "cache_read_input_tokens": 14721, "output_tokens": 21 },
                "modelUsage": { "claude-haiku-5-5": { "inputTokens": 4, "outputTokens": 35, "costUSD": 0.00911025, "contextWindow": 1000000 } } }),
        ]
    }

    #[test]
    fn claude_turns_take_their_own_usage_and_their_share_of_the_running_cost() {
        let [r1, r2] = claude_results();
        let mut m = ClaudeMeter::default();
        m.on_result(&r1);
        let t1 = m.take();
        assert_eq!(
            (t1.input, t1.output, t1.cache_read, t1.cache_write),
            (2, 14, 11854, 17588)
        );
        assert_eq!(t1.model.as_deref(), Some("claude-haiku-5-5"));
        m.on_result(&r2);
        let t2 = m.take();
        assert_eq!(
            (t2.input, t2.output, t2.cache_write),
            (2, 21, 26545),
            "this turn's, not the running total"
        );
        assert!((t2.cost_usd.unwrap() - (0.00911025 - 0.00364334)).abs() < 1e-12);
    }

    #[test]
    fn resumed_claude_seeds_history_without_charging_it_to_the_new_session() {
        let mut m = ClaudeMeter::resumed();
        m.on_result(&json!({"usage":{},"total_cost_usd":139.60,"modelUsage":{"claude-opus-5-5":{"costUSD":139.60}}}));
        assert_eq!(m.take().cost_usd, Some(0.0));
        m.on_result(&json!({"usage":{"input_tokens":10},"total_cost_usd":139.65}));
        assert!((m.take().cost_usd.unwrap() - 0.05).abs() < 1e-10);
        let mut m = ClaudeMeter::resumed();
        m.on_result(&json!({"usage":{"input_tokens":1000},"total_cost_usd":140.0,"modelUsage":{"claude-opus-5-5":{"costUSD":140.0}}}));
        assert!(
            (m.take().cost_usd.unwrap() - 0.004).abs() < 1e-10,
            "first new calls are estimated, not lifetime cost"
        );
    }

    #[test]
    fn a_steered_prompt_adds_up_its_results() {
        let [r1, r2] = claude_results();
        let mut m = ClaudeMeter::default();
        m.on_result(&r1);
        m.on_result(&r2);
        let t = m.take();
        assert_eq!(t.output, 35);
        assert!((t.cost_usd.unwrap() - 0.00911025).abs() < 1e-12);
        assert!(m.take().is_empty());
    }

    #[test]
    fn a_new_claude_process_starts_its_running_cost_over() {
        let [_, r2] = claude_results();
        let mut m = ClaudeMeter::default();
        m.on_result(&r2);
        m.take();
        let [r1, _] = claude_results();
        m.on_result(&r1);
        assert!(
            (m.take().cost_usd.unwrap() - 0.00364334).abs() < 1e-12,
            "lower than before: a restart"
        );
    }

    #[test]
    fn claude_context_comes_from_the_main_agents_last_response() {
        let mut m = ClaudeMeter::default();
        let assistant = |usage: Value, parent: Value| json!({ "type": "assistant", "parent_tool_use_id": parent, "message": { "model": "claude-haiku-5-5", "usage": usage } });
        m.on_assistant(&assistant(json!({ "input_tokens": 2, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 50, "output_tokens": 8 }), Value::Null));
        m.on_assistant(&assistant(
            json!({ "input_tokens": 9000, "output_tokens": 1 }),
            json!("toolu_1"),
        ));
        assert_eq!(m.context(), None, "no size before a result");
        m.on_result(&claude_results()[0]);
        assert_eq!(
            m.context(),
            Some((160, 1_000_000)),
            "a subagent's response is not the main context"
        );
    }

    #[test]
    fn codex_turns_are_what_the_running_total_grew_by() {
        let report = |total: [u64; 5], last: [u64; 5]| {
            let b = |v: [u64; 5]| json!({ "inputTokens": v[0], "cachedInputTokens": v[1], "cacheWriteInputTokens": v[2], "outputTokens": v[3], "reasoningOutputTokens": v[4] });
            json!({ "total": b(total), "last": b(last), "modelContextWindow": 272000 })
        };
        let mut m = CodexMeter {
            model: Some("gpt-5.5".into()),
            ..Default::default()
        };
        // A resumed thread: the first report's total holds its history; only `last` is new.
        m.observe(&report(
            [50_000, 40_000, 0, 5_000, 1_000],
            [1_000, 800, 0, 100, 20],
        ));
        m.observe(&report(
            [52_000, 41_500, 0, 5_300, 1_050],
            [2_000, 1_500, 0, 300, 50],
        ));
        let t = m.take();
        assert_eq!(t.cache_read, 800 + 1_500);
        assert_eq!(t.input, 3_000 - 2_300, "input excludes what was cached");
        assert_eq!((t.output, t.reasoning), (400, 70));
        assert_eq!(t.model.as_deref(), Some("gpt-5.5"));
        // A total that went down (a fresh count): `last` only.
        m.observe(&report([10, 0, 0, 5, 0], [10, 0, 0, 5, 0]));
        let t = m.take();
        assert_eq!((t.input, t.output), (10, 5));
    }

    #[test]
    fn usage_travels_in_the_prompt_reply() {
        let t = TurnUsage {
            input: 2,
            output: 14,
            cache_read: 11854,
            cache_write: 17588,
            reasoning: 3,
            cost_usd: Some(0.0036),
            model: Some("claude-haiku-5-5".into()),
        };
        let reply = t.clone().into_reply(json!({ "stopReason": "end_turn" }));
        assert_eq!(reply["stopReason"], "end_turn");
        assert_eq!(reply["usage"]["totalTokens"], 2 + 14 + 11854 + 17588);
        assert_eq!(TurnUsage::from_reply(&reply), Some(t));
        assert_eq!(
            TurnUsage::default().into_reply(json!({ "stopReason": "end_turn" })),
            json!({ "stopReason": "end_turn" }),
            "nothing spent, nothing added"
        );
        assert_eq!(
            TurnUsage::from_reply(&json!({ "stopReason": "end_turn" })),
            None
        );
        let mut foreign = reply.clone();
        foreign.as_object_mut().unwrap().remove("_meta");
        assert_eq!(
            TurnUsage::from_reply(&foreign),
            None,
            "another agent's usage may count differently"
        );
    }

    #[test]
    fn claudes_rate_limit_event_gives_its_plan_windows() {
        // As the real `claude -p` printed it.
        let ev: Value = serde_json::from_str(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793858400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.13,"resetsAt":1793858400,"status":"allowed"},"seven_day":{"utilization":0.09,"resetsAt":1794366000,"status":"allowed"}}}}"#).unwrap();
        let mut l = claude_limits(&ev);
        l.sort_by_key(|l| l.window.as_str());
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0].window, l[0].resets_at),
            (LimitWindow::FiveHour, Some(1793858400))
        );
        assert!((l[0].used_pct - 13.0).abs() < 1e-9);
        assert_eq!(l[1].window, LimitWindow::Week);
        assert!(claude_limits(&json!({ "type": "rate_limit_event" })).is_empty());
    }

    #[test]
    fn codex_rate_limits_give_its_plan_windows() {
        let read = json!({ "rateLimits": { "limitId": "codex", "planType": "plus",
            "primary": { "usedPercent": 42.0, "windowDurationMins": 300, "resetsAt": 1000 },
            "secondary": { "usedPercent": 7.5, "resetsAt": 2000 } } });
        let l = codex_limits(&read);
        assert_eq!(
            l,
            vec![
                Limit {
                    window: LimitWindow::FiveHour,
                    used_pct: 42.0,
                    resets_at: Some(1000)
                },
                Limit {
                    window: LimitWindow::Week,
                    used_pct: 7.5,
                    resets_at: Some(2000)
                },
            ]
        );
        // A model's own limit isn't the plan's; Codex's entry wins when listed.
        assert!(codex_limits(&json!({ "rateLimits": { "limitId": "gpt-big", "primary": { "usedPercent": 90.0 } } })).is_empty());
        let listed = json!({ "rateLimits": { "limitId": "gpt-big", "primary": { "usedPercent": 90.0 } }, "rateLimitsByLimitId": { "codex": { "primary": { "usedPercent": 3.0 } } } });
        assert_eq!(codex_limits(&listed)[0].used_pct, 3.0);
        // A business plan, as a real `account/rateLimits/read` replied (trimmed):
        // no windows, a monthly credit allowance.
        let business: Value = serde_json::from_str(r#"{"rateLimits":{"limitId":"codex","primary":null,"secondary":null,"individualLimit":{"limit":"50000","used":"15.1","remainingPercent":100,"resetsAt":1796083200},"planType":"business"},"rateLimitsByLimitId":{"codex":{"limitId":"codex","primary":null,"secondary":null,"individualLimit":{"limit":"50000","used":"15.1","remainingPercent":72,"resetsAt":1796083200},"planType":"business"}}}"#).unwrap();
        assert_eq!(
            codex_limits(&business),
            vec![Limit {
                window: LimitWindow::Month,
                used_pct: 28.0,
                resets_at: Some(1796083200)
            }]
        );
        // Free plans' first window is the month.
        assert_eq!(
            codex_limits(
                &json!({ "rateLimits": { "planType": "free", "primary": { "usedPercent": 1.0 } } })
            )[0]
            .window,
            LimitWindow::Month
        );
    }

    #[test]
    fn limits_travel_to_the_host() {
        let l = vec![Limit {
            window: LimitWindow::Week,
            used_pct: 9.0,
            resets_at: None,
        }];
        assert_eq!(limits_from_params(&limits_params(&l)), l);
    }
}
