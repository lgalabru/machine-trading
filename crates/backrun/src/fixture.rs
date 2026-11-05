//! Public market metadata never contains the private generation seed or flow.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Result, bail, ensure};
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    ConstantProduct,
    Concentrated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub kind: PoolKind,
    pub address: String,
    pub config: String,
    pub mints: [usize; 2],
    pub vaults: [String; 2],
    pub observation: String,
    pub fee_millionths: u32,
    pub tick_spacing: u16,
    pub tick_arrays: Vec<String>,
    /// Resolved in memory after reading Market; not duplicated in JSON.
    #[serde(skip)]
    pub token_addresses: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Market {
    pub version: u32,
    pub tokens: Vec<String>,
    pub pools: Vec<Pool>,
    pub program_hashes: BTreeMap<String, String>,
}
impl Market {
    pub fn hydrate(&mut self) {
        for p in &mut self.pools {
            p.token_addresses = self.tokens.clone();
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.tokens.len() == 24 && self.pools.len() == 128,
            "expected v1, 24 tokens and 128 pools"
        );
        ensure!(
            self.tokens[0] == crate::WSOL,
            "settlement token must be WSOL"
        );
        let mut keys = BTreeSet::new();
        for mint in &self.tokens {
            mint.parse::<solana_pubkey::Pubkey>()?;
            ensure!(keys.insert(mint), "duplicate mint");
        }
        let mut pools = BTreeSet::new();
        for p in &self.pools {
            ensure!(
                p.mints[0] < 24 && p.mints[1] < 24 && p.mints[0] != p.mints[1],
                "invalid edge"
            );
            p.address.parse::<solana_pubkey::Pubkey>()?;
            for address in [&p.config, &p.observation]
                .into_iter()
                .chain(p.vaults.iter())
                .chain(p.tick_arrays.iter())
            {
                address.parse::<solana_pubkey::Pubkey>()?;
            }
            ensure!(pools.insert(&p.address), "duplicate pool");
            ensure!(p.fee_millionths < 1_000_000, "invalid fee");
            ensure!(
                match p.kind {
                    PoolKind::ConstantProduct => p.tick_spacing == 0 && p.tick_arrays.is_empty(),
                    PoolKind::Concentrated => p.tick_spacing > 0 && p.tick_arrays.len() == 8,
                },
                "invalid pool layout"
            );
        }
        ensure!(
            self.pools
                .iter()
                .filter(|p| p.kind == PoolKind::ConstantProduct)
                .count()
                == 64,
            "expected 64 CPMMs"
        );
        let mut reached = BTreeSet::from([0]);
        loop {
            let before = reached.len();
            for p in &self.pools {
                if reached.contains(&p.mints[0]) || reached.contains(&p.mints[1]) {
                    reached.extend(p.mints);
                }
            }
            if reached.len() == before {
                break;
            }
        }
        ensure!(reached.len() == 24, "disconnected graph");
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    pub pool: usize,
    pub a_to_b: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserOrder {
    pub slot: u64,
    pub route: Vec<Leg>,
    pub amount_in: u64,
    pub min_out: u64,
    pub wallet: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateFlow {
    pub version: u32,
    pub duration_slots: u64,
    pub orders: Vec<UserOrder>,
}

impl PrivateFlow {
    pub fn validate(&self, market: &Market) -> Result<()> {
        ensure!(
            self.version == 1 && self.duration_slots == 2000,
            "unsupported evaluation schedule"
        );
        let mut previous = 0;
        for order in &self.orders {
            ensure!(
                order.slot >= previous && order.slot < self.duration_slots,
                "unordered or out-of-range event"
            );
            ensure!(
                order.route.len() == 1
                    && order.amount_in > 0
                    && order.min_out > 0
                    && order.wallet < 8,
                "invalid user order"
            );
            let mut token = None;
            for leg in &order.route {
                let p = market
                    .pools
                    .get(leg.pool)
                    .ok_or_else(|| anyhow::anyhow!("unknown pool"))?;
                let input = p.mints[usize::from(!leg.a_to_b)];
                if let Some(expected) = token {
                    ensure!(expected == input, "disconnected user route");
                }
                token = Some(p.mints[usize::from(leg.a_to_b)]);
            }
            previous = order.slot;
        }
        Ok(())
    }
}

/// Connected ring + chords + parallel venues. The same seed is used only at
/// fixture generation; no grading-time randomness chooses topology or flow.
pub fn topology(seed: [u8; 32]) -> Vec<[usize; 2]> {
    let mut rng = StdRng::from_seed(seed);
    let mut edges = Vec::new();
    let mut pairs = BTreeSet::new();
    for i in 0..24 {
        let mut p = [i, (i + 1) % 24];
        p.sort();
        pairs.insert(p);
        edges.push(p);
    }
    while edges.len() < 64 {
        let mut p = [rng.random_range(0..24), rng.random_range(0..24)];
        p.sort();
        if p[0] != p[1] && pairs.insert(p) {
            edges.push(p);
        }
    }
    edges.extend(edges.clone());
    edges
}

pub fn hash_file(path: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(fs::read(path)?)))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seal {
    pub version: u32,
    pub files: BTreeMap<String, String>,
    pub fixture_hash: String,
}

pub fn seal(root: &Path, files: &[&str]) -> Result<Seal> {
    let mut hashes = BTreeMap::new();
    for name in files {
        hashes.insert((*name).into(), hash_file(&root.join(name))?);
    }
    let fixture_hash = hex::encode(Sha256::digest(serde_json::to_vec(&hashes)?));
    Ok(Seal {
        version: 1,
        files: hashes,
        fixture_hash,
    })
}

pub fn verify_seal(root: &Path, expected: &Seal) -> Result<()> {
    ensure!(expected.version == 1, "unknown seal version");
    for (name, digest) in &expected.files {
        let path = Path::new(name);
        if path.is_absolute()
            || path.components().count() != 1
            || path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            bail!("unsafe fixture path");
        }
        ensure!(
            !fs::symlink_metadata(root.join(path))?
                .file_type()
                .is_symlink(),
            "symlink fixture"
        );
        ensure!(
            hash_file(&root.join(path))? == *digest,
            "fixture content changed: {name}"
        );
    }
    ensure!(
        hex::encode(Sha256::digest(serde_json::to_vec(&expected.files)?)) == expected.fixture_hash,
        "fixture seal changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topology_has_parallel_venues_and_alternative_paths() {
        let edges = topology([9; 32]);
        assert_eq!(edges.len(), 128);
        assert_eq!(&edges[..64], &edges[64..]);
        // Removing any one edge cannot disconnect the ring backbone.
        for omitted in 0..64 {
            let mut reachable = BTreeSet::from([0]);
            for _ in 0..24 {
                for (i, p) in edges[..64].iter().enumerate() {
                    if i != omitted && (reachable.contains(&p[0]) || reachable.contains(&p[1])) {
                        reachable.extend(p);
                    }
                }
            }
            assert_eq!(reachable.len(), 24);
        }
    }
    #[test]
    fn fixture_mutation_is_detected() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("flow.json"), b"private").unwrap();
        let sealed = seal(temp.path(), &["flow.json"]).unwrap();
        verify_seal(temp.path(), &sealed).unwrap();
        fs::write(temp.path().join("flow.json"), b"tampered").unwrap();
        assert!(verify_seal(temp.path(), &sealed).is_err());
    }
}
