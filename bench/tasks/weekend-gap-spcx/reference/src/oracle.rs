//! The ladder's path oracle: told the perp's whole path (the reopen's
//! included), it plans the position turn by turn that makes the most after
//! costs, by dynamic programming over a grid of positions. Following the
//! path naively loses money: the perp's wobble pays for every flip. Plain
//! Rust, shared by the bot and the offline tooling.

/// Positions it plans on, in shares, and the most it moves in a turn.
const GRID: f64 = 5.0;

/// The planned position after each turn's trade, given the perp's level at
/// each turn (percent of `start_usd`), the cost of trading in basis points
/// (half-spread plus fee) and its growth per share traded, the cap and the
/// most it may trade a turn.
pub fn plan(
    levels: &[f64],
    start_usd: f64,
    cost_bps: f64,
    depth_bps_per_share: f64,
    cap: f64,
    max_step: f64,
) -> Vec<f64> {
    let n = (cap / GRID).floor() as i64;
    let states: Vec<f64> = (-n..=n).map(|i| i as f64 * GRID).collect();
    let k = levels.len();
    if k == 0 {
        return Vec::new();
    }
    let s = states.len();
    // value[t][j]: the best from turn t on, holding states[j] before
    // trading at turn t.
    let mut value = vec![0.0; s];
    let mut choice = vec![vec![0usize; s]; k];
    for t in (0..k).rev() {
        let price = start_usd * (1.0 + levels[t] / 100.0);
        let carry = if t + 1 < k {
            levels[t + 1] - levels[t]
        } else {
            0.0
        };
        let mut next = vec![f64::MIN; s];
        for (j, &held) in states.iter().enumerate() {
            for (m, &to) in states.iter().enumerate() {
                let q = (to - held).abs();
                if q > max_step + 1e-9 {
                    continue;
                }
                let cost = q * price * (cost_bps + depth_bps_per_share * q) / 1e4;
                let gain = to * start_usd * carry / 100.0;
                let v = gain - cost + value[m];
                if v > next[j] {
                    next[j] = v;
                    choice[t][j] = m;
                }
            }
        }
        value = next;
    }
    let mut at = n as usize;
    let mut out = Vec::with_capacity(k);
    for row in &choice {
        at = row[at];
        out.push(states[at]);
    }
    out
}
