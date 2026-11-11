// Forecasts are trained offline. All inference is local and uses only prices
// observed through the current turn.
pub const MODELS: &[(&[u8], f64)] = &[(include_bytes!("forecast.bin"), 1.0)];
pub const ENTRY_THRESHOLD: f64 = 0.001;
pub const HOLD_THRESHOLD: f64 = -0.001;
