//! Tunable parameters. Defaults are what the grader runs; environment
//! overrides (`BOT_<NAME>`) are for sweeps in the practice environment.

/// `KEY=VALUE` lines from `params.env` next to the bot directory (for sweeps).
fn file_overrides() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(exe) = std::env::current_exe() else { return map };
    let Some(dir) = exe.parent().and_then(|p| p.parent()) else { return map };
    if let Ok(text) = std::fs::read_to_string(dir.join("params.env")) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

macro_rules! params {
    ($($name:ident : $t:ty = $default:expr),* $(,)?) => {
        #[derive(Debug, Clone)]
        pub struct Params { $(pub $name: $t),* }
        impl Params {
            pub fn from_env() -> Self {
                let file = file_overrides();
                let get = |name: &str| -> Option<String> {
                    std::env::var(format!("BOT_{name}")).ok().or_else(|| file.get(name).cloned())
                };
                Self { $($name: get(stringify!($name))
                    .and_then(|v| v.parse::<$t>().ok()).unwrap_or($default)),* }
            }
        }
    };
}

params! {
    // Quote size in base units per side.
    quote_size: f64 = 1.5,
    // Minimum distance from the oracle as a fraction of the competitor half-spread.
    min_frac: f64 = 0.4,
    // Minimum distance in ticks regardless of the fraction.
    min_ticks: i64 = 3,
    // Maximum distance as a fraction of the competitor half-spread.
    max_frac: f64 = 1.0,
    // Inventory band for ordinary making (base units); sizes taper toward the band edge.
    inv_band: f64 = 8.0,
    // Quote skew per unit of inventory excess, in ticks per base unit.
    skew_ticks: f64 = 0.6,
    // Hard position cap safety margin (base units below MTX_MAX_POSITION).
    cap_margin: f64 = 0.6,
    // Informed-flow alert: window in slots, one-sided volume, order count, ratio to the other side.
    burst_window: u64 = 12,
    burst_volume: f64 = 5.0,
    burst_count: u64 = 3,
    burst_ratio: f64 = 3.0,
    // Bonus volume credited when the competitor's side was wiped out.
    wipe_bonus: f64 = 1.5,
    // How long an alert lasts after the last qualifying order, in slots.
    alert_slots: u64 = 30,
    // Caution (softer) level: window, single-order size, volume; effect and duration.
    caution_window: u64 = 8,
    caution_size: f64 = 1.8,
    caution_volume: f64 = 3.5,
    caution_size_mult: f64 = 0.4,
    caution_widen: i64 = 2,
    caution_slots: u64 = 8,
    // Lean target (base units) into a burst's direction; 0 disables.
    burst_lean: f64 = 0.0,
    // Take-to-flatten when the inventory is against an alert and beyond this size.
    flatten_against: f64 = 1.0,
    // Worst taking distance as a fraction of the competitor half-spread (leans / flattening against an alert).
    take_max_frac: f64 = 1.5,
    flatten_max_frac: f64 = 2.5,
    // Trend lean: z to start leaning, z at full lean, full lean size, flow weight.
    trend_z_on: f64 = 2.2,
    trend_z_full: f64 = 4.0,
    trend_lean: f64 = 14.0,
    flow_weight: f64 = 0.7,
    // Take toward the lean target when further than this, at most every N steps.
    lean_take_gap: f64 = 3.0,
    lean_take_every: u64 = 10,
    // Quote this many ticks inside the competitor (0 = tie with it).
    undercut_ticks: i64 = 1,
    // Reduce-side quotes may go this close to the oracle (ticks), once the excess passes the threshold.
    reduce_min_ticks: i64 = 2,
    reduce_threshold: f64 = 1.0,
    // Jump arbitrage: take resting orders crossed with the oracle by at least this many ticks.
    arb_min_ticks: i64 = 10,
}
