//! `mtx_kit::protocols::raydium::clmm` on real mainnet state, and against
//! the program itself.
//!
//! `fixtures/raydium-clmm-sol-usdc/accounts.json.gz` holds, at one slot
//! (472303248), the SOL/USDC pool (`3ucNos4…`, 4 bp, tick spacing 1), its
//! config, observation and bitmap extension, and its 13 initialized tick
//! arrays nearest the price, as base64. It was fetched with one
//! `getProgramAccounts` (the pool's tick arrays, sliced to their start
//! index) and one `getMultipleAccounts`.
//!
//! `swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts` executes swaps
//! on a Surfpool fork of mainnet and compares every balance to the quote:
//!
//! ```text
//! surfpool start --rpc-url <mainnet> --port 28899 --no-tui --no-deploy --yes
//! MTX_SURFNET_URL=http://127.0.0.1:28899 cargo test -p mtx-kit \
//!   --features raydium-clmm --test raydium_clmm -- --ignored --nocapture
//! ```
#![cfg(feature = "raydium-clmm")]

use std::{collections::HashMap, io::Read as _, str::FromStr as _};

use base64::Engine as _;
use mtx_kit::protocols::raydium::clmm::{
    AmmConfig, BitmapExtension, ClmmError, Pool, SOL_USDC_POOL, SwapDirection, SwapMode, SwapQuote,
    SwapRequest, TickArray, ix, quote, tick_arrays_in_direction,
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

/// After the pool's open time; the pool has no dynamic fee, so the exact
/// value doesn't matter.
const BLOCK_TIMESTAMP_S: u64 = 1_763_700_000;
const SOL_ATOMS: u64 = 1_000_000_000;
const USDC_ATOMS: u64 = 1_000_000;

struct Market {
    address: Pubkey,
    pool: Pool,
    config: AmmConfig,
    extension: BitmapExtension,
    tick_arrays: Vec<TickArray>,
}

impl Market {
    fn quote(
        &self,
        direction: SwapDirection,
        mode: SwapMode,
        amount: u64,
    ) -> Result<SwapQuote, ClmmError> {
        let request = SwapRequest {
            direction,
            mode,
            amount,
            sqrt_price_limit_x64: 0,
            block_timestamp_s: BLOCK_TIMESTAMP_S,
        };
        quote(
            &self.address,
            &self.pool,
            &self.config,
            &self.tick_arrays,
            Some(&self.extension),
            &request,
        )
    }
}

fn fixture() -> Market {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/raydium-clmm-sol-usdc/accounts.json.gz"
    );
    let mut json = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).expect("fixture"))
        .read_to_string(&mut json)
        .expect("gzip");
    let fixture: Fixture = serde_json::from_str(&json).expect("fixture json");
    assert_eq!(fixture.slot, 472_303_248);
    let address = Pubkey::from_str(&fixture.pool).expect("pool address");
    let bytes = |key: &Pubkey| -> Vec<u8> {
        let account = &fixture.accounts[&key.to_string()];
        base64::engine::general_purpose::STANDARD
            .decode(&account.data)
            .expect("base64")
    };
    let pool = Pool::decode(&bytes(&address)).expect("pool");
    let config = AmmConfig::decode(&bytes(&pool.amm_config)).expect("config");
    let extension_address = ix::bitmap_extension_address(&address);
    let extension = BitmapExtension::decode(&bytes(&extension_address)).expect("extension");
    let mut tick_arrays = Vec::new();
    for key in fixture.accounts.keys() {
        let key = Pubkey::from_str(key).expect("key");
        let data = bytes(&key);
        if data.len() == 10240 {
            let array = TickArray::decode(&data).expect("tick array");
            assert_eq!(
                ix::tick_array_address(&address, array.start_tick_index),
                key
            );
            tick_arrays.push(array);
        }
    }
    tick_arrays.sort_by_key(|array| array.start_tick_index);
    Market {
        address,
        pool,
        config,
        extension,
        tick_arrays,
    }
}

#[test]
fn the_fixture_decodes_as_the_sol_usdc_pool() {
    let market = fixture();
    let pool = &market.pool;
    assert_eq!(market.address, SOL_USDC_POOL);
    assert_eq!(
        pool.token_mint_0,
        Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap()
    );
    assert_eq!(
        pool.token_mint_1,
        Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap()
    );
    assert_eq!(
        (
            pool.mint_decimals_0,
            pool.mint_decimals_1,
            pool.tick_spacing
        ),
        (9, 6, 1)
    );
    assert_eq!(pool.tick_current, -26576);
    assert!(pool.dynamic_fee.is_none());
    assert_eq!(market.config.trade_fee_rate, 400);
    assert_eq!(market.config.tick_spacing, 1);
    assert_eq!(market.extension.pool, market.address);
    assert_eq!(market.tick_arrays.len(), 13);
    // The price is about $70 a SOL: sqrt(price) in USDC atoms per lamport.
    let price = (pool.sqrt_price_x64 as f64 / 2f64.powi(64)).powi(2) * 1e3;
    assert!((50.0..100.0).contains(&price), "{price}");
    // Every array the navigation lists from the price is in the fixture.
    for direction in [SwapDirection::ZeroForOne, SwapDirection::OneForZero] {
        let starts = tick_arrays_in_direction(pool, Some(&market.extension), direction, 6).unwrap();
        assert_eq!(starts.len(), 6);
        for start in starts {
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
            SwapDirection::ZeroForOne,
            [SOL_ATOMS / 100, 100 * SOL_ATOMS, 2_000 * SOL_ATOMS],
        ),
        (
            SwapDirection::OneForZero,
            [USDC_ATOMS, 10_000 * USDC_ATOMS, 150_000 * USDC_ATOMS],
        ),
    ];
    for (direction, amounts) in cases {
        let mut previous: Option<SwapQuote> = None;
        for amount in amounts {
            let exact_in = market.quote(direction, SwapMode::ExactIn, amount).unwrap();
            assert_eq!(exact_in.amount_in, amount);
            // The fee is 4 bp of the input, rounded up per step.
            let fee_floor = amount / 2_500;
            assert!(
                exact_in.fee_amount >= fee_floor && exact_in.fee_amount <= fee_floor + 64,
                "{exact_in:?}"
            );
            // Buying what selling `amount` yields costs no more than it, and
            // less by at most what one more atom of output would cost.
            let exact_out = market
                .quote(direction, SwapMode::ExactOut, exact_in.amount_out)
                .unwrap();
            assert_eq!(exact_out.amount_out, exact_in.amount_out);
            let input_per_output_atom = amount.div_ceil(exact_in.amount_out);
            assert!(exact_out.amount_in <= amount, "{exact_out:?}");
            assert!(
                amount - exact_out.amount_in <= input_per_output_atom + 2,
                "{exact_out:?}"
            );
            // More in, more out, at a worse price, further from the start.
            if let Some(previous) = &previous {
                assert!(exact_in.amount_out > previous.amount_out);
                assert!(
                    u128::from(exact_in.amount_out) * u128::from(previous.amount_in)
                        <= u128::from(previous.amount_out) * u128::from(exact_in.amount_in)
                );
                assert!(exact_in.ticks_crossed >= previous.ticks_crossed);
            }
            previous = Some(exact_in);
        }
        // The largest swap crosses ticks in several tick arrays.
        let largest = previous.unwrap();
        assert!(largest.ticks_crossed > 10, "{largest:?}");
        assert!(largest.tick_arrays.len() >= 2, "{largest:?}");
        assert!(!largest.needs_bitmap_extension);
        assert_eq!(
            largest.tick_arrays,
            largest
                .tick_array_start_indexes
                .iter()
                .map(|&start| ix::tick_array_address(&market.address, start))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_swap_beyond_the_fixture_names_the_tick_array_it_lacks() {
    let market = fixture();
    match market.quote(
        SwapDirection::ZeroForOne,
        SwapMode::ExactIn,
        1_000_000 * SOL_ATOMS,
    ) {
        Err(ClmmError::MissingTickArray { start_tick_index }) => assert!(start_tick_index < -26940),
        other => panic!("{other:?}"),
    }
}

/// Swaps of several sizes, both ways, exact in and exact out, on a
/// Surfpool fork of mainnet: every vault and wallet balance must move by
/// exactly the quote, and `swap_v2` is given the quote itself as its
/// threshold, so a quote one atom too generous would fail the swap.
///
/// Besides SOL/USDC, `MTX_RAYDIUM_POOLS` may list other pools (comma
/// separated), swapped at fractions of their vaults: pools that take their
/// fee in the output token or charge a dynamic fee exercise what SOL/USDC
/// doesn't.
#[test]
#[ignore = "needs a Surfpool fork of mainnet at MTX_SURFNET_URL"]
fn swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts() {
    let url = std::env::var("MTX_SURFNET_URL").expect("MTX_SURFNET_URL");
    let fork = surfnet::Fork::new(&url);
    fork.fund(&SOL_USDC_POOL);
    let cases = [
        (
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            SOL_ATOMS / 100,
        ),
        (SwapDirection::OneForZero, SwapMode::ExactIn, USDC_ATOMS),
        (
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            1_500 * SOL_ATOMS,
        ),
        (
            SwapDirection::OneForZero,
            SwapMode::ExactIn,
            120_000 * USDC_ATOMS,
        ),
        (
            SwapDirection::ZeroForOne,
            SwapMode::ExactOut,
            30_000 * USDC_ATOMS,
        ),
        (
            SwapDirection::OneForZero,
            SwapMode::ExactOut,
            1_000 * SOL_ATOMS,
        ),
        (
            SwapDirection::ZeroForOne,
            SwapMode::ExactOut,
            7 * USDC_ATOMS,
        ),
        (SwapDirection::OneForZero, SwapMode::ExactIn, 100),
    ];
    for (direction, mode, amount) in cases {
        println!(
            "SOL/USDC {}",
            fork.swap_and_compare(&SOL_USDC_POOL, direction, mode, amount)
        );
    }
    let extra = std::env::var("MTX_RAYDIUM_POOLS").unwrap_or_default();
    for pool in extra.split(',').filter(|pool| !pool.is_empty()) {
        let pool = Pubkey::from_str(pool.trim()).expect("pool address");
        fork.fund(&pool);
        // Fractions of the vault the amount is taken from, in basis points.
        for (direction, mode, vault_bps) in [
            (SwapDirection::ZeroForOne, SwapMode::ExactIn, 1),
            (SwapDirection::OneForZero, SwapMode::ExactIn, 300),
            (SwapDirection::ZeroForOne, SwapMode::ExactOut, 200),
            (SwapDirection::OneForZero, SwapMode::ExactOut, 2),
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
    use mtx_kit::protocols::raydium::clmm::{
        AmmConfig, BitmapExtension, COMPUTE_UNIT_LIMIT, Pool, SwapDirection, SwapMode, SwapRequest,
        TickArray, ix, quote, tick_arrays_in_direction,
    };
    use solana_commitment_config::CommitmentConfig;
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    use solana_keypair::Keypair;
    use solana_pubkey::Pubkey;
    use solana_rpc_client::rpc_client::RpcClient;
    use solana_rpc_client_api::request::RpcRequest;
    use solana_signer::Signer as _;
    use solana_transaction::Transaction;

    /// Tick arrays fetched ahead of a swap: more than the largest case
    /// enters.
    const TICK_ARRAYS_FETCHED: usize = 16;
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

        /// Gives the trader twice each of the pool's vaults (a quarter of
        /// `u64::MAX` at most, so that receiving can't overflow).
        pub fn fund(&self, pool_address: &Pubkey) {
            let pool = Pool::decode(&self.data(pool_address)).expect("pool");
            let programs = self.mint_programs(&pool);
            for (mint, vault, program) in [
                (pool.token_mint_0, pool.token_vault_0, programs.token_0),
                (pool.token_mint_1, pool.token_vault_1, programs.token_1),
            ] {
                let amount = self
                    .token_amount(&vault)
                    .saturating_mul(2)
                    .clamp(1_000_000_000_000_000, u64::MAX / 4);
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
            let pool = Pool::decode(&self.data(pool_address)).expect("pool");
            let is_vault_0 =
                (direction == SwapDirection::ZeroForOne) == (mode == SwapMode::ExactIn);
            let vault = if is_vault_0 {
                pool.token_vault_0
            } else {
                pool.token_vault_1
            };
            (u128::from(self.token_amount(&vault)) * u128::from(vault_bps) / 10_000).max(1) as u64
        }

        fn mint_programs(&self, pool: &Pool) -> ix::MintPrograms {
            let owner = |mint: &Pubkey| self.rpc.get_account(mint).expect("mint").owner;
            ix::MintPrograms {
                token_0: owner(&pool.token_mint_0),
                token_1: owner(&pool.token_mint_1),
            }
        }

        fn data(&self, key: &Pubkey) -> Vec<u8> {
            self.rpc.get_account(key).expect("account").data
        }

        fn token_amount(&self, key: &Pubkey) -> u64 {
            let data = self.data(key);
            u64::from_le_bytes(data[64..72].try_into().expect("token account"))
        }

        pub fn swap_and_compare(
            &self,
            address: &Pubkey,
            direction: SwapDirection,
            mode: SwapMode,
            amount: u64,
        ) -> String {
            let address = *address;
            let pool = Pool::decode(&self.data(&address)).expect("pool");
            let config = AmmConfig::decode(&self.data(&pool.amm_config)).expect("config");
            let extension_address = ix::bitmap_extension_address(&address);
            let extension = self
                .rpc
                .get_account(&extension_address)
                .ok()
                .map(|account| BitmapExtension::decode(&account.data).expect("extension"));
            let starts =
                tick_arrays_in_direction(&pool, extension.as_ref(), direction, TICK_ARRAYS_FETCHED)
                    .expect("tick arrays");
            let keys: Vec<Pubkey> = starts
                .iter()
                .map(|&start| ix::tick_array_address(&address, start))
                .collect();
            let tick_arrays: Vec<TickArray> = self
                .rpc
                .get_multiple_accounts(&keys)
                .expect("tick arrays")
                .into_iter()
                .flatten()
                .map(|account| TickArray::decode(&account.data).expect("tick array"))
                .collect();
            let clock = self.data(&CLOCK_SYSVAR);
            let block_timestamp_s =
                i64::from_le_bytes(clock[32..40].try_into().expect("clock")) as u64;
            let request = SwapRequest {
                direction,
                mode,
                amount,
                sqrt_price_limit_x64: 0,
                block_timestamp_s,
            };
            let owner = self.trader.pubkey();
            let programs = self.mint_programs(&pool);
            let quote = match quote(
                &address,
                &pool,
                &config,
                &tick_arrays,
                extension.as_ref(),
                &request,
            ) {
                Ok(quote) => quote,
                Err(refusal) => {
                    // The program must refuse it too, even given every
                    // tick array fetched and no threshold.
                    let swap = ix::swap_v2(
                        &owner,
                        &address,
                        &pool,
                        &programs,
                        extension.is_some().then_some(&extension_address),
                        &keys[..keys.len().min(tick_arrays.len()).max(1)],
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
            let user_0 = mtx_kit::token::associated_token_address(
                &owner,
                &pool.token_mint_0,
                &programs.token_0,
            );
            let user_1 = mtx_kit::token::associated_token_address(
                &owner,
                &pool.token_mint_1,
                &programs.token_1,
            );
            let watched = [pool.token_vault_0, pool.token_vault_1, user_0, user_1];
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
                extension.is_some().then_some(&extension_address),
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
                panic!("{direction:?} {mode:?} {amount}: swap failed: {err:?}\nquote {quote:?}")
            });
            let after: Vec<u64> = watched.iter().map(|key| self.token_amount(key)).collect();
            let delta = |i: usize| i128::from(after[i]) - i128::from(before[i]);
            let (amount_in, amount_out) =
                (i128::from(quote.amount_in), i128::from(quote.amount_out));
            let expected = match direction {
                SwapDirection::ZeroForOne => [amount_in, -amount_out, -amount_in, amount_out],
                SwapDirection::OneForZero => [-amount_out, amount_in, amount_out, -amount_in],
            };
            let actual = [delta(0), delta(1), delta(2), delta(3)];
            assert_eq!(
                actual, expected,
                "{direction:?} {mode:?} {amount}: {signature}\nquote {quote:?}"
            );
            format!(
                "{direction:?} {mode:?} {amount}: in {} out {} fee {} ({:?}) ticks crossed {} limit orders {} arrays {} -> matched",
                quote.amount_in,
                quote.amount_out,
                quote.fee_amount,
                quote.fee_token,
                quote.ticks_crossed,
                quote.limit_order_amount_out,
                quote.tick_arrays.len()
            )
        }
    }

    impl Fork {
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
