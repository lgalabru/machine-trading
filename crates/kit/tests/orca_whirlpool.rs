//! `mtx_kit::protocols::orca::whirlpool` on real mainnet state, and against
//! the program itself.
//!
//! `fixtures/orca-whirlpool-sol-usdc/accounts.json.gz` holds, at one slot
//! (472310732), the SOL/USDC pool (`Czfq3xZ…`, 4 bp, tick spacing 4) and
//! the 9 of its tick arrays nearest the price (fixed and dynamic), as
//! base64. Its oracle doesn't exist: the pool's fee is static. It was
//! fetched with one `getProgramAccounts` per layout (the pool's tick
//! arrays, sliced to their start index) and one `getMultipleAccounts`.
//!
//! `swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts` executes swaps
//! on a Surfpool fork of mainnet and compares every balance to the quote:
//!
//! ```text
//! surfpool start --rpc-url <mainnet> --port 18899 --no-tui --no-deploy --yes
//! MTX_SURFNET_URL=http://127.0.0.1:18899 cargo test -p mtx-kit \
//!   --features orca-whirlpool --test orca_whirlpool -- --ignored --nocapture
//! ```
#![cfg(feature = "orca-whirlpool")]

use std::{collections::HashMap, io::Read as _, str::FromStr as _};

use base64::Engine as _;
use mtx_kit::protocols::orca::whirlpool::{
    SOL_USDC_POOL, SwapDirection, SwapMode, SwapQuote, SwapRequest, TickArray, TickArrayLayout,
    Whirlpool, WhirlpoolError, ix, quote, tick_array_addresses,
};
use serde::Deserialize;
use solana_pubkey::Pubkey;

#[derive(Deserialize)]
struct Fixture {
    slot: u64,
    pool: String,
    accounts: HashMap<String, FixtureAccount>,
}

#[derive(Deserialize)]
struct FixtureAccount {
    data: String,
}

/// The pool's fee is static, so the time only has to be plausible.
const TIMESTAMP_S: u64 = 1_763_700_000;
const SOL_ATOMS: u64 = 1_000_000_000;
const USDC_ATOMS: u64 = 1_000_000;
const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
const USDC_MINT: Pubkey = solana_pubkey::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

struct Market {
    address: Pubkey,
    pool: Whirlpool,
    tick_arrays: Vec<TickArray>,
}

impl Market {
    fn quote(
        &self,
        direction: SwapDirection,
        mode: SwapMode,
        amount: u64,
    ) -> Result<SwapQuote, WhirlpoolError> {
        let request = SwapRequest {
            direction,
            mode,
            amount,
            sqrt_price_limit_x64: 0,
            timestamp_s: TIMESTAMP_S,
        };
        quote(&self.address, &self.pool, &self.tick_arrays, None, &request)
    }
}

fn fixture() -> Market {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/orca-whirlpool-sol-usdc/accounts.json.gz"
    );
    let mut json = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).expect("fixture"))
        .read_to_string(&mut json)
        .expect("gzip");
    let fixture: Fixture = serde_json::from_str(&json).expect("fixture json");
    assert_eq!(fixture.slot, 472_310_732);
    let address = Pubkey::from_str(&fixture.pool).expect("pool address");
    assert_eq!(address, SOL_USDC_POOL);
    let mut pool = None;
    let mut tick_arrays = Vec::new();
    for (key, account) in &fixture.accounts {
        let key = Pubkey::from_str(key).expect("key");
        let data = base64::engine::general_purpose::STANDARD
            .decode(&account.data)
            .expect("base64");
        if key == address {
            pool = Some(Whirlpool::decode(&data).expect("pool"));
            continue;
        }
        let array = TickArray::decode(&data).expect("tick array");
        assert_eq!(array.whirlpool, address);
        assert_eq!(
            ix::tick_array_address(&address, array.start_tick_index),
            key
        );
        tick_arrays.push(array);
    }
    tick_arrays.sort_by_key(|array| array.start_tick_index);
    Market {
        address,
        pool: pool.expect("the fixture holds the pool"),
        tick_arrays,
    }
}

#[test]
fn the_fixture_decodes_as_the_sol_usdc_pool() {
    let market = fixture();
    let pool = &market.pool;
    assert_eq!(
        (pool.token_mint_a, pool.token_mint_b),
        (WRAPPED_SOL_MINT, USDC_MINT)
    );
    assert_eq!((pool.tick_spacing, pool.fee_rate), (4, 400));
    assert!(!pool.is_adaptive_fee_tier());
    assert!(pool.liquidity > 0);
    // SOL between $50 and $1,000: 10^-3 USDC atoms per lamport times that.
    let price = (pool.sqrt_price_x64 as f64 / 2f64.powi(64)).powi(2) * 1e3;
    assert!((50.0..1_000.0).contains(&price), "price {price}");
    assert_eq!(market.tick_arrays.len(), 9);
    let layouts: Vec<TickArrayLayout> = market.tick_arrays.iter().map(|a| a.layout).collect();
    assert!(
        layouts.contains(&TickArrayLayout::Fixed) || layouts.contains(&TickArrayLayout::Dynamic)
    );
    let spans = market
        .tick_arrays
        .windows(2)
        .all(|pair| pair[1].start_tick_index - pair[0].start_tick_index == pool.ticks_in_array());
    assert!(spans, "the fixture's arrays are contiguous");
    for direction in [SwapDirection::AToB, SwapDirection::BToA] {
        for (start, _) in tick_array_addresses(&market.address, pool, direction) {
            assert!(
                market
                    .tick_arrays
                    .iter()
                    .any(|array| array.start_tick_index == start)
            );
        }
    }
}

#[test]
fn quotes_are_consistent_across_sizes_directions_and_modes() {
    let market = fixture();
    let cases = [
        (
            SwapDirection::AToB,
            [1_000, SOL_ATOMS / 10, 100 * SOL_ATOMS, 2_000 * SOL_ATOMS],
        ),
        (
            SwapDirection::BToA,
            [
                1_000,
                10 * USDC_ATOMS,
                20_000 * USDC_ATOMS,
                300_000 * USDC_ATOMS,
            ],
        ),
    ];
    let mut crossed_several = false;
    for (direction, amounts) in cases {
        let mut previous_out = 0;
        for amount in amounts {
            let exact_in = market
                .quote(direction, SwapMode::ExactIn, amount)
                .expect("exact in");
            assert_eq!(
                exact_in.amount_in, amount,
                "{direction:?} {amount}: fills entirely"
            );
            assert!(exact_in.amount_out >= previous_out, "more in, no less out");
            previous_out = exact_in.amount_out;
            // 4 bp, rounded up once per step.
            let fee_floor = u128::from(amount) * 400 / 1_000_000;
            let fee = u128::from(exact_in.fee_amount);
            assert!(fee + 1 >= fee_floor, "{direction:?} {amount}: fee {fee}");
            assert!(
                fee <= fee_floor + u128::from(exact_in.ticks_crossed) + 2,
                "{direction:?} {amount}: fee {fee}"
            );
            match direction {
                SwapDirection::AToB => {
                    assert!(exact_in.sqrt_price_x64_end <= market.pool.sqrt_price_x64)
                }
                SwapDirection::BToA => {
                    assert!(exact_in.sqrt_price_x64_end >= market.pool.sqrt_price_x64)
                }
            }
            crossed_several |= exact_in.ticks_crossed >= 3;

            // Asking for exactly that output costs no more than it paid.
            if exact_in.amount_out > 0 {
                let exact_out = market
                    .quote(direction, SwapMode::ExactOut, exact_in.amount_out)
                    .expect("exact out");
                assert_eq!(exact_out.amount_out, exact_in.amount_out);
                assert!(
                    exact_out.amount_in <= exact_in.amount_in,
                    "{direction:?} {amount}"
                );
                // And one atom more costs more: exact in leaves no output
                // atom on the table.
                let one_more = market
                    .quote(direction, SwapMode::ExactOut, exact_in.amount_out + 1)
                    .expect("one atom more");
                assert!(
                    one_more.amount_in > exact_in.amount_in,
                    "{direction:?} {amount}"
                );
            }
        }
    }
    assert!(
        crossed_several,
        "the largest swaps cross several initialized ticks"
    );
}

#[test]
fn a_swap_beyond_three_arrays_fails_and_a_missing_array_is_named() {
    let market = fixture();
    let too_big = market.quote(
        SwapDirection::AToB,
        SwapMode::ExactIn,
        10_000_000 * SOL_ATOMS,
    );
    assert!(
        matches!(too_big, Err(WhirlpoolError::Program(_))),
        "{too_big:?}"
    );

    let starts: Vec<i32> = tick_array_addresses(&market.address, &market.pool, SwapDirection::BToA)
        .into_iter()
        .map(|(start, _)| start)
        .collect();
    let without_second = Market {
        tick_arrays: market
            .tick_arrays
            .iter()
            .filter(|array| array.start_tick_index != starts[1])
            .cloned()
            .collect(),
        ..market
    };
    assert_eq!(
        without_second.quote(SwapDirection::BToA, SwapMode::ExactIn, USDC_ATOMS),
        Err(WhirlpoolError::MissingTickArray {
            start_tick_index: starts[1]
        })
    );
}

#[test]
fn an_adaptive_fee_pool_needs_its_oracle() {
    let mut market = fixture();
    market.pool.fee_tier_index = 1028;
    assert_eq!(
        market.quote(SwapDirection::AToB, SwapMode::ExactIn, SOL_ATOMS),
        Err(WhirlpoolError::MissingOracle)
    );
}

/// Swaps on a Surfpool fork of mainnet (`MTX_SURFNET_URL`): each must move
/// the two vaults and the trader's two token accounts by exactly the
/// quoted amounts. The swap's threshold is the quote's own amount, so a
/// quote one atom too generous fails the swap. `MTX_WHIRLPOOLS` adds
/// pools (comma-separated addresses), such as adaptive-fee ones, which
/// trade sizes in proportion to their vaults.
#[test]
#[ignore = "needs a Surfpool fork of mainnet at MTX_SURFNET_URL"]
fn swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts() {
    let url = std::env::var("MTX_SURFNET_URL").expect("MTX_SURFNET_URL");
    let fork = surfnet::Fork::new(&url);
    fork.fund(&SOL_USDC_POOL);
    let cases = [
        (SwapDirection::AToB, SwapMode::ExactIn, SOL_ATOMS / 100),
        (SwapDirection::BToA, SwapMode::ExactIn, USDC_ATOMS),
        (SwapDirection::AToB, SwapMode::ExactIn, 1_000 * SOL_ATOMS),
        (SwapDirection::BToA, SwapMode::ExactIn, 150_000 * USDC_ATOMS),
        (SwapDirection::AToB, SwapMode::ExactOut, 40_000 * USDC_ATOMS),
        (SwapDirection::BToA, SwapMode::ExactOut, 500 * SOL_ATOMS),
        (SwapDirection::AToB, SwapMode::ExactOut, 7 * USDC_ATOMS),
        (SwapDirection::BToA, SwapMode::ExactIn, 100),
        (SwapDirection::AToB, SwapMode::ExactIn, 1),
        // Several arrays: 352 ticks each, about 3.5% of the price.
        (SwapDirection::AToB, SwapMode::ExactIn, 20_000 * SOL_ATOMS),
        (SwapDirection::BToA, SwapMode::ExactOut, 20_000 * SOL_ATOMS),
        (
            SwapDirection::BToA,
            SwapMode::ExactIn,
            3_000_000 * USDC_ATOMS,
        ),
        (
            SwapDirection::AToB,
            SwapMode::ExactOut,
            2_500_000 * USDC_ATOMS,
        ),
    ];
    for (direction, mode, amount) in cases {
        println!(
            "SOL/USDC {}",
            fork.swap_and_compare(&SOL_USDC_POOL, direction, mode, amount)
        );
    }
    let extra = std::env::var("MTX_WHIRLPOOLS").unwrap_or_default();
    for pool in extra.split(',').filter(|pool| !pool.is_empty()) {
        let pool = Pubkey::from_str(pool.trim()).expect("pool address");
        fork.fund(&pool);
        // Fractions of the vault the amount is taken from, in basis points.
        for (direction, mode, vault_bps) in [
            (SwapDirection::AToB, SwapMode::ExactIn, 1),
            (SwapDirection::BToA, SwapMode::ExactIn, 300),
            (SwapDirection::AToB, SwapMode::ExactOut, 200),
            (SwapDirection::BToA, SwapMode::ExactOut, 2),
        ] {
            let amount = fork.vault_fraction(&pool, direction, mode, vault_bps);
            println!(
                "{pool} {}",
                fork.swap_and_compare(&pool, direction, mode, amount)
            );
        }
    }
}

/// The fork's RPC, the cheatcodes that fund a fresh trader, and one swap.
mod surfnet {
    use mtx_kit::protocols::orca::whirlpool::{
        COMPUTE_UNIT_LIMIT, Oracle, SwapDirection, SwapMode, SwapRequest, TickArray, Whirlpool, ix,
        quote, tick_array_addresses,
    };
    use solana_commitment_config::CommitmentConfig;
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    use solana_keypair::Keypair;
    use solana_pubkey::Pubkey;
    use solana_rpc_client::rpc_client::RpcClient;
    use solana_rpc_client_api::request::RpcRequest;
    use solana_signer::Signer as _;
    use solana_transaction::Transaction;

    const CLOCK_SYSVAR: Pubkey =
        solana_pubkey::pubkey!("SysvarC1ock11111111111111111111111111111111");

    pub struct Fork {
        rpc: RpcClient,
        trader: Keypair,
    }

    impl Fork {
        pub fn new(url: &str) -> Self {
            let rpc =
                RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed());
            let trader = Keypair::new();
            cheat(
                &rpc,
                "surfnet_setAccount",
                serde_json::json!([trader.pubkey().to_string(), {"lamports": 10_000_000_000u64}]),
            );
            Self { rpc, trader }
        }

        /// Gives the trader twice each of the pool's vaults, within the
        /// mint's plausible range.
        pub fn fund(&self, pool_address: &Pubkey) {
            let pool = Whirlpool::decode(&self.data(pool_address)).expect("pool");
            let programs = self.mint_programs(&pool);
            for (mint, vault, program) in [
                (pool.token_mint_a, pool.token_vault_a, programs.token_a),
                (pool.token_mint_b, pool.token_vault_b, programs.token_b),
            ] {
                let amount = self
                    .token_amount(&vault)
                    .saturating_mul(2)
                    .max(1_000_000_000_000);
                cheat(
                    &self.rpc,
                    "surfnet_setTokenAccount",
                    serde_json::json!([self.trader.pubkey().to_string(), mint.to_string(), {"amount": amount}, program.to_string()]),
                );
            }
        }

        /// `vault_bps` of the vault the swap's fixed amount comes from or
        /// goes to.
        pub fn vault_fraction(
            &self,
            pool_address: &Pubkey,
            direction: SwapDirection,
            mode: SwapMode,
            vault_bps: u64,
        ) -> u64 {
            let pool = Whirlpool::decode(&self.data(pool_address)).expect("pool");
            let is_vault_a = (direction == SwapDirection::AToB) == (mode == SwapMode::ExactIn);
            let vault = if is_vault_a {
                pool.token_vault_a
            } else {
                pool.token_vault_b
            };
            (u128::from(self.token_amount(&vault)) * u128::from(vault_bps) / 10_000).max(1) as u64
        }

        fn mint_programs(&self, pool: &Whirlpool) -> ix::MintPrograms {
            let owner = |mint: &Pubkey| self.rpc.get_account(mint).expect("mint").owner;
            ix::MintPrograms {
                token_a: owner(&pool.token_mint_a),
                token_b: owner(&pool.token_mint_b),
            }
        }

        fn data(&self, key: &Pubkey) -> Vec<u8> {
            self.rpc.get_account(key).expect("account").data
        }

        fn token_amount(&self, key: &Pubkey) -> u64 {
            let data = self.data(key);
            u64::from_le_bytes(data[64..72].try_into().expect("token account"))
        }

        fn timestamp_s(&self) -> u64 {
            let clock = self.data(&CLOCK_SYSVAR);
            i64::from_le_bytes(clock[32..40].try_into().expect("clock")) as u64
        }

        pub fn swap_and_compare(
            &self,
            address: &Pubkey,
            direction: SwapDirection,
            mode: SwapMode,
            amount: u64,
        ) -> String {
            let address = *address;
            let pool = Whirlpool::decode(&self.data(&address)).expect("pool");
            let keys = tick_array_addresses(&address, &pool, direction);
            let addresses: Vec<Pubkey> = keys.iter().map(|&(_, key)| key).collect();
            let tick_arrays: Vec<TickArray> = self
                .rpc
                .get_multiple_accounts(&addresses)
                .expect("tick arrays")
                .into_iter()
                .zip(&keys)
                .map(|(account, &(start, _))| match account {
                    Some(account) => TickArray::decode(&account.data).expect("tick array"),
                    None => TickArray::uninitialized(address, start),
                })
                .collect();
            let oracle = self
                .rpc
                .get_account(&ix::oracle_address(&address))
                .ok()
                .map(|account| Oracle::decode(&account.data).expect("oracle"));
            let request = SwapRequest {
                direction,
                mode,
                amount,
                sqrt_price_limit_x64: 0,
                timestamp_s: self.timestamp_s(),
            };
            let owner = self.trader.pubkey();
            let programs = self.mint_programs(&pool);
            let fee_kind = if oracle.is_some() {
                "adaptive"
            } else {
                "static"
            };
            let layouts: Vec<String> = tick_arrays
                .iter()
                .map(|array| format!("{:?}", array.layout))
                .collect();
            let quote = match quote(&address, &pool, &tick_arrays, oracle.as_ref(), &request) {
                Ok(quote) => quote,
                Err(refusal) => {
                    // The program must refuse it too, with no threshold.
                    let mut accounts = [addresses[0]; 3];
                    for (slot, key) in addresses.iter().enumerate() {
                        accounts[slot] = *key;
                    }
                    let swap = ix::swap_v2(
                        &owner,
                        &address,
                        &pool,
                        &programs,
                        &accounts,
                        &ix::SwapV2Args {
                            direction,
                            mode,
                            amount,
                            other_amount_threshold: if mode == SwapMode::ExactIn {
                                0
                            } else {
                                u64::MAX
                            },
                            sqrt_price_limit_x64: 0,
                        },
                    );
                    let result = self.send(swap);
                    assert!(
                        result.is_err(),
                        "{direction:?} {mode:?} {amount}: quote refused ({refusal}), the program didn't"
                    );
                    return format!(
                        "{direction:?} {mode:?} {amount}: quote and program both refuse ({refusal})"
                    );
                }
            };
            let user_a = mtx_kit::token::associated_token_address(
                &owner,
                &pool.token_mint_a,
                &programs.token_a,
            );
            let user_b = mtx_kit::token::associated_token_address(
                &owner,
                &pool.token_mint_b,
                &programs.token_b,
            );
            let watched = [pool.token_vault_a, pool.token_vault_b, user_a, user_b];
            let before: Vec<u64> = watched.iter().map(|key| self.token_amount(key)).collect();

            let threshold = match mode {
                SwapMode::ExactIn => quote.amount_out,
                SwapMode::ExactOut => quote.amount_in,
            };
            let swap = ix::swap_v2(
                &owner,
                &address,
                &pool,
                &programs,
                &quote.tick_arrays,
                &ix::SwapV2Args {
                    direction,
                    mode,
                    amount,
                    other_amount_threshold: threshold,
                    sqrt_price_limit_x64: 0,
                },
            );
            let signature = self.send(swap).unwrap_or_else(|err| {
                panic!("{direction:?} {mode:?} {amount}: swap failed: {err}\nquote {quote:?}")
            });
            let after: Vec<u64> = watched.iter().map(|key| self.token_amount(key)).collect();
            let delta = |i: usize| i128::from(after[i]) - i128::from(before[i]);
            let (amount_in, amount_out) =
                (i128::from(quote.amount_in), i128::from(quote.amount_out));
            let expected = match direction {
                SwapDirection::AToB => [amount_in, -amount_out, -amount_in, amount_out],
                SwapDirection::BToA => [-amount_out, amount_in, amount_out, -amount_in],
            };
            let actual = [delta(0), delta(1), delta(2), delta(3)];
            assert_eq!(
                actual, expected,
                "{direction:?} {mode:?} {amount}: {signature}\nquote {quote:?}"
            );
            let end = Whirlpool::decode(&self.data(&address)).expect("pool");
            assert_eq!(
                (end.sqrt_price_x64, end.tick_current_index, end.liquidity),
                (
                    quote.sqrt_price_x64_end,
                    quote.tick_end,
                    quote.liquidity_end
                ),
                "{direction:?} {mode:?} {amount}: the pool's end state"
            );
            format!(
                "{direction:?} {mode:?} {amount} ({fee_kind} fee): in {} out {} fee {} ticks crossed {} arrays {} of {} -> matched",
                quote.amount_in,
                quote.amount_out,
                quote.fee_amount,
                quote.ticks_crossed,
                quote.tick_arrays_entered,
                layouts.join("/")
            )
        }

        fn send(&self, swap: solana_instruction::Instruction) -> Result<String, String> {
            let blockhash = self.rpc.get_latest_blockhash().expect("blockhash");
            let transaction = Transaction::new_signed_with_payer(
                &[
                    ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT),
                    swap,
                ],
                Some(&self.trader.pubkey()),
                &[&self.trader],
                blockhash,
            );
            self.rpc
                .send_and_confirm_transaction(&transaction)
                .map(|signature| signature.to_string())
                .map_err(|err| format!("{err:?}"))
        }
    }

    fn cheat(rpc: &RpcClient, method: &'static str, params: serde_json::Value) {
        let _: serde_json::Value = rpc
            .send(RpcRequest::Custom { method }, params)
            .unwrap_or_else(|err| panic!("{method}: {err:?}"));
    }
}
