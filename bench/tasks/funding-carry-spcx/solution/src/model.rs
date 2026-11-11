// Embedded regression trees trained on independently generated market paths.
pub struct Node {
    feature: usize,
    leaf: bool,
    threshold: f64,
    value: f64,
    left: usize,
    right: usize,
}
pub struct Model {
    base: f64,
    trees: Vec<Vec<Node>>,
}
impl Model {
    pub fn load() -> Self {
        let b = include_bytes!("../model.bin");
        let mut p = 0;
        fn u(b: &[u8], p: &mut usize) -> u32 {
            let x = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            x
        }
        fn d(b: &[u8], p: &mut usize) -> f64 {
            let x = f64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
            *p += 8;
            x
        }
        let n = u(b, &mut p);
        let base = d(b, &mut p);
        let mut trees = Vec::new();
        for _ in 0..n {
            let n = u(b, &mut p);
            let mut tree = Vec::new();
            for _ in 0..n {
                tree.push(Node {
                    feature: u(b, &mut p) as usize,
                    leaf: u(b, &mut p) != 0,
                    threshold: d(b, &mut p),
                    value: d(b, &mut p),
                    left: u(b, &mut p) as usize,
                    right: u(b, &mut p) as usize,
                });
            }
            trees.push(tree);
        }
        Self { base, trees }
    }
    pub fn predict(&self, x: &[f64]) -> f64 {
        let mut y = self.base;
        for tree in &self.trees {
            let mut n = 0;
            loop {
                let node = &tree[n];
                if node.leaf {
                    y += node.value;
                    break;
                }
                n = if x[node.feature] as f32 as f64 <= node.threshold {
                    node.left
                } else {
                    node.right
                };
            }
        }
        y
    }
}

pub struct Features {
    hist: Vec<(f64, f64, f64)>,
    sign: f64,
    active: bool,
    start: usize,
    base: f64,
    bookbase: f64,
    opening: f64,
    peak: f64,
    inf: bool,
    quiet: usize,
    counts: usize,
    informed: usize,
    lasttick: usize,
    last_gap: f64,
    last_idx: f64,
    last_book: f64,
    tickdelta: [f64; 3],
}
impl Features {
    pub fn new() -> Self {
        Self {
            hist: Vec::new(),
            sign: 1.0,
            active: false,
            start: 0,
            base: 0.0,
            bookbase: 0.0,
            opening: 0.0,
            peak: 0.0,
            inf: false,
            quiet: 0,
            counts: 0,
            informed: 0,
            lasttick: 0,
            last_gap: 0.0,
            last_idx: 0.0,
            last_book: 0.0,
            tickdelta: [0.0; 3],
        }
    }
    pub fn update(
        &mut self,
        index: f64,
        book: f64,
        period: f64,
        cap_per_second_bps: f64,
    ) -> (Vec<f64>, f64) {
        let k = self.hist.len();
        let g = book / index - 1.0;
        if k == 0 {
            self.base = index;
            self.bookbase = book;
            self.last_idx = index;
            self.last_book = book;
        }
        if g.abs() < 0.0012 {
            self.quiet += 1;
        } else {
            self.quiet = 0;
        }
        if self.active && (self.quiet >= 6 || g * self.sign < -0.002) {
            self.active = false;
            self.informed += usize::from(self.inf);
        }
        if !self.active && g.abs() > 0.0025 {
            self.active = true;
            self.sign = g.signum();
            self.start = k;
            self.base = index;
            self.bookbase = book;
            self.opening = g.abs();
            self.peak = g.abs();
            self.inf = false;
            self.counts += 1;
        }
        if self.active {
            self.peak = self.peak.max(g.abs());
            if (index / self.base - 1.0) * self.sign > 0.002f64.max(self.peak * 0.3) {
                self.inf = true;
            }
        }
        if k > 0 && (index != self.hist[k - 1].0 || book != self.hist[k - 1].1) {
            self.tickdelta = [
                (g - self.last_gap) * self.sign * 10000.0,
                (index / self.last_idx - 1.0) * self.sign * 10000.0,
                (book / self.last_book - 1.0) * self.sign * 10000.0,
            ];
            self.last_gap = g;
            self.last_idx = index;
            self.last_book = book;
            self.lasttick = k;
        }
        self.hist.push((index, book, g));
        let active = self.active;
        let mut x = vec![
            g * self.sign * 10000.0,
            g.abs() * 10000.0,
            period,
            cap_per_second_bps,
            if active {
                (k - self.start) as f64 * 0.5
            } else {
                0.0
            },
            if active { self.opening * 10000.0 } else { 0.0 },
            if active { self.peak * 10000.0 } else { 0.0 },
            if active {
                (index / self.base - 1.0) * self.sign * 10000.0
            } else {
                0.0
            },
            if active {
                (book / self.bookbase - 1.0) * self.sign * 10000.0
            } else {
                0.0
            },
            if active {
                g.abs() / (self.peak + 1e-8)
            } else {
                0.0
            },
            if self.inf && active { 1.0 } else { 0.0 },
            self.quiet as f64 * 0.5,
            self.counts as f64,
            (self.informed as f64 + 0.5) / (self.counts.saturating_sub(1) as f64 + 1.0),
        ];
        for lag in [1, 2, 4, 8, 12, 20, 32, 48] {
            let p = self.hist[k.saturating_sub(lag)];
            x.extend_from_slice(&[
                (index / p.0 - 1.0) * self.sign * 10000.0,
                (book / p.1 - 1.0) * self.sign * 10000.0,
                (g - p.2) * self.sign * 10000.0,
            ]);
        }
        let window = &self.hist[k.saturating_sub(64)..];
        let ih = window.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let il = window.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let bh = window.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let bl = window.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        x.extend_from_slice(&[
            (index / ih - 1.0) * self.sign * 10000.0,
            (index / il - 1.0) * self.sign * 10000.0,
            (book / bh - 1.0) * self.sign * 10000.0,
            (book / bl - 1.0) * self.sign * 10000.0,
            (k - self.lasttick) as f64 * 0.5,
        ]);
        x.extend_from_slice(&self.tickdelta);
        (x, self.sign)
    }
}
