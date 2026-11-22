//! An atomic group's lines joined into one transaction
//! (`docs/adr/0004-atomic-arbitrage-in-the-sheet.md`): the lines of a sheet
//! that share `tx-options.atomic`, in sheet order, land together or not at
//! all.
//!
//! The transaction is laid out so that every index is final before
//! anything refers to one:
//!
//! 1. The compute budget: the limit, then the price where the session
//!    applies one. The host places it, so the transaction layer only
//!    changes it in place and never moves an instruction.
//! 2. Every `push-price` line, in sheet order: Scope takes a refresh only
//!    right after the compute budget.
//! 3. The `flash` line's borrow (and the token account it lends into).
//! 4. The other lines, in sheet order, each as its seat builds it.
//! 5. The flash loan's repay, which names the borrow by its index.
//!
//! The compute limit is the lines' own limits added up, capped at what a
//! transaction may ask for, unless the first line's options set one. The
//! lookup tables are every line's, once each. The first line's seat signs
//! and sends it: every seat of a session signs with the strategy's key.

use mtx_kit::{
    Action, Planned, TxOptions,
    chain::{AddressLookupTableAccount, InstructionGroup, TransactionPlan},
};
use solana_transaction::Instruction;

use crate::venue::{Built, Seat};

/// The most compute units a transaction may ask for: the runtime's limit.
pub const COMPUTE_UNIT_LIMIT_MAX: u32 = 1_400_000;

/// The transaction of `group`, an atomic group's planned lines in sheet
/// order, and the seat that sends it, or why it can't be built. `seat` finds
/// a line's seat by its venue; `options` are the group's first line's,
/// where the session applies them.
pub async fn build<'a>(
    group: &[Planned],
    seat: impl Fn(Option<&str>) -> Option<&'a dyn Seat>,
    options: Option<TxOptions>,
) -> Result<(TransactionPlan, &'a dyn Seat), String> {
    assert!(!group.is_empty(), "a group has lines");
    let id = group[0].tx.and_then(|tx| tx.atomic);
    assert!(
        group.iter().all(|p| p.tx.and_then(|tx| tx.atomic) == id),
        "a group's lines share its id"
    );
    let mut seats = Vec::with_capacity(group.len());
    for planned in group {
        let venue = planned.venue.as_deref();
        let found = seat(venue).ok_or_else(|| {
            format!(
                "line {}: {} isn't traded in this session",
                planned.line,
                venue.unwrap_or("the session's market")
            )
        })?;
        if !found.executes(&planned.action) {
            return Err(format!(
                "line {}: {} doesn't execute {:?}",
                planned.line,
                found.market(),
                planned.action
            ));
        }
        seats.push(found);
    }
    let lines: Vec<(&Planned, &dyn Seat)> = group.iter().zip(seats.iter().copied()).collect();
    let flashes: Vec<_> = lines
        .iter()
        .filter(|(p, _)| matches!(p.action, Action::Flash { .. }))
        .collect();
    if flashes.len() > 1 {
        return Err("a group holds at most one flash loan".into());
    }
    let pushes = built(lines.iter().filter(|(p, _)| p.action == Action::PushPrice)).await?;
    let body = built(
        lines
            .iter()
            .filter(|(p, _)| !matches!(p.action, Action::Flash { .. } | Action::PushPrice)),
    )
    .await?;
    let price = options.and_then(|o| o.cu_price);
    let budget = 1 + usize::from(price.is_some());
    let pushed: usize = pushes.iter().map(|b| b.instructions.len()).sum();
    let at = u8::try_from(budget + pushed)
        .map_err(|_| "the group has more instructions than a transaction can name")?;
    let flash = match flashes.first() {
        Some((planned, seat)) => Some(
            seat.flash(planned, at)
                .await
                .map_err(|e| format!("line {}: {e}", planned.line))?,
        ),
        None => None,
    };
    let (before, after): (Vec<Built>, Vec<Instruction>) = match flash {
        Some(flash) => (vec![flash.borrow], flash.repay),
        None => (Vec::new(), Vec::new()),
    };
    let parts: Vec<&Built> = pushes.iter().chain(&before).chain(&body).collect();
    let plan = join(&parts, after, options, id.unwrap_or_default());
    assert_eq!(
        plan.budget_len(),
        budget,
        "the budget is what the flash was told"
    );
    if plan.all_instructions().len() > usize::from(u8::MAX) {
        return Err("the group has more instructions than a transaction can name".into());
    }
    Ok((plan, seats[0]))
}

/// Each line's instructions, from its seat, in order; the first that can't
/// be built refuses the group.
async fn built<'b>(
    lines: impl Iterator<Item = &'b (&'b Planned, &'b dyn Seat)>,
) -> Result<Vec<Built>, String> {
    let mut out = Vec::new();
    for (planned, seat) in lines {
        out.push(
            seat.instructions(planned)
                .await
                .map_err(|e| format!("line {}: {e}", planned.line))?,
        );
    }
    Ok(out)
}

/// `parts`, in order, then `after`, as one transaction: its compute the
/// parts' added up and capped, or the options' own limit; its price the
/// options'; every part's lookup tables, once each.
fn join(
    parts: &[&Built],
    after: Vec<Instruction>,
    options: Option<TxOptions>,
    id: u32,
) -> TransactionPlan {
    let instructions: Vec<Instruction> = parts
        .iter()
        .flat_map(|b| b.instructions.iter().cloned())
        .chain(after)
        .collect();
    let added = parts
        .iter()
        .fold(0u32, |sum, b| sum.saturating_add(b.compute_unit_limit))
        .min(COMPUTE_UNIT_LIMIT_MAX);
    let limit = options
        .and_then(|o| o.cu_limit)
        .unwrap_or(added)
        .min(COMPUTE_UNIT_LIMIT_MAX);
    let mut tables: Vec<AddressLookupTableAccount> = Vec::new();
    for table in parts.iter().flat_map(|b| &b.lookup_tables) {
        if tables.iter().all(|t| t.key != table.key) {
            tables.push(table.clone());
        }
    }
    let labels: Vec<&str> = parts.iter().map(|b| b.label.as_str()).collect();
    let label = format!("atomic group {id}: {}", labels.join(" + "));
    let plan = TransactionPlan::from_group(&InstructionGroup::new(label, instructions), limit)
        .with_lookup_tables(&tables);
    match options.and_then(|o| o.cu_price) {
        Some(price) => plan.with_compute_unit_price(price),
        None => plan,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use mtx_core::{
        Asset,
        mandate::{InstrumentRef, Kind},
    };
    use mtx_kit::{
        AmountUnit, Sent, Side,
        chain::{mock::MockChainClient, token},
        token::TOKEN_PROGRAM_ID,
    };
    use solana_account::Account;
    use solana_keypair::Keypair;
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_transaction::AccountMeta;

    use super::*;
    use crate::{
        desk::seat_in,
        venue::{
            SeatResult,
            kamino::{
                Book, Books, LendingSeat,
                klend::{self, RESERVE_LEN, ReserveKeys},
                scope,
            },
            sent_plan,
        },
    };

    /// The program a pool seat's swaps call here: each swap is one
    /// instruction whose data is its line, so the order shows.
    const SWAP_PROGRAM: Pubkey = Pubkey::new_from_array([7; 32]);
    const COMPUTE_BUDGET: Pubkey =
        solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");

    /// A pool's seat that builds a swap as one marked instruction, or
    /// refuses to build.
    struct Pool {
        market: String,
        chain: Arc<MockChainClient>,
        signer: Keypair,
        builds: bool,
    }

    #[async_trait]
    impl Seat for Pool {
        fn market(&self) -> &str {
            &self.market
        }

        fn executes(&self, action: &Action) -> bool {
            matches!(action, Action::Swap { .. })
        }

        async fn send(&self, _planned: &Planned) -> Sent {
            unreachable!("a group goes out through send_plan")
        }

        async fn instructions(&self, planned: &Planned) -> SeatResult<Built> {
            if !self.builds {
                return Err("the pool moved past the limit".into());
            }
            let line = u8::try_from(planned.line).expect("a short sheet");
            Ok(Built {
                instructions: vec![Instruction::new_with_bytes(
                    SWAP_PROGRAM,
                    &[line],
                    vec![AccountMeta::new(self.signer.pubkey(), true)],
                )],
                lookup_tables: Vec::new(),
                compute_unit_limit: 200_000,
                label: format!("swap {line}"),
            })
        }

        async fn send_plan(&self, plan: &TransactionPlan) -> Sent {
            sent_plan(self.chain.as_ref(), &self.signer, plan).await
        }

        async fn position(&self) -> SeatResult<f64> {
            Ok(0.0)
        }
    }

    fn owned(owner: Pubkey, data: Vec<u8>) -> Account {
        Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// A SOL reserve on `chain`, priced by Scope entry 5 of a feed there,
    /// and the lending seat of `signer` on it.
    fn lender(chain: &Arc<MockChainClient>, signer: &Keypair) -> LendingSeat {
        let keys = ReserveKeys {
            address: Pubkey::new_unique(),
            market: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            supply_vault: Pubkey::new_unique(),
            fee_vault: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            collateral_vault: Pubkey::new_unique(),
            token_program: TOKEN_PROGRAM_ID,
            farm_collateral: Pubkey::default(),
            farm_debt: Pubkey::default(),
            scope_prices: Pubkey::new_unique(),
        };
        let mut reserve = vec![0u8; RESERVE_LEN];
        reserve[..8].copy_from_slice(&klend::account_tag("Reserve"));
        reserve[5112..5144].copy_from_slice(keys.scope_prices.as_ref());
        // A price chain of entry 5 and no TWAP: the push refreshes 5 alone.
        let chains = [5u16, u16::MAX, u16::MAX, u16::MAX]
            .into_iter()
            .chain([u16::MAX; 4]);
        for (i, entry) in chains.enumerate() {
            reserve[5144 + 2 * i..5146 + 2 * i].copy_from_slice(&entry.to_le_bytes());
        }
        let (mappings, twaps, source) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut prices = vec![0u8; 40];
        prices[8..40].copy_from_slice(mappings.as_ref());
        let mut mapped = vec![0u8; 8 + 32 * scope::ENTRIES_MAX];
        mapped[8 + 32 * 5..8 + 32 * 6].copy_from_slice(source.as_ref());
        let mut twapped = vec![0u8; 72];
        twapped[8..40].copy_from_slice(keys.scope_prices.as_ref());
        twapped[40..72].copy_from_slice(mappings.as_ref());
        chain
            .set_account(keys.address, owned(klend::PROGRAM_ID, reserve))
            .set_account(keys.scope_prices, owned(scope::PROGRAM_ID, prices))
            .set_account(mappings, owned(scope::PROGRAM_ID, mapped))
            .set_account(twaps, owned(scope::PROGRAM_ID, twapped));
        let book = Book {
            instrument: InstrumentRef {
                venue: "kamino".into(),
                kind: Kind::MoneyMarket,
                symbol: "SOL".into(),
            },
            keys,
            asset: Asset::new(keys.mint.to_string(), "SOL", 9).expect("an asset"),
            decimals: 9,
        };
        let books = Arc::new(Books {
            reserves: vec![book.clone()],
            lookup_tables: Default::default(),
        });
        LendingSeat::new(book, books, chain.clone(), signer.insecure_clone())
    }

    fn planned(line: usize, venue: &str, action: Action, options: TxOptions) -> Planned {
        Planned {
            line,
            venue: Some(venue.into()),
            client_id: None,
            action,
            reduce_only: false,
            expires_slot: None,
            tx: Some(TxOptions {
                atomic: Some(1),
                ..options
            }),
        }
    }

    fn swap(side: Side) -> Action {
        Action::Swap {
            side,
            amount: 10.0,
            unit: AmountUnit::Base,
            limit: 150.0,
        }
    }

    /// A flash-funded route with a price push, as a strategy may write it:
    /// the push and the loan anywhere among the swaps.
    fn route(options: TxOptions) -> Vec<Planned> {
        vec![
            planned(0, "orca:amm:A", swap(Side::Ask), options),
            planned(
                1,
                "kamino:money-market:SOL",
                Action::Flash { amount: 10.0 },
                options,
            ),
            planned(2, "kamino:money-market:SOL", Action::PushPrice, options),
            planned(3, "raydium:amm:B", swap(Side::Bid), options),
        ]
    }

    fn seats(chain: &Arc<MockChainClient>, signer: &Keypair, builds: bool) -> Vec<Box<dyn Seat>> {
        let pool = |market: &str| Pool {
            market: market.into(),
            chain: chain.clone(),
            signer: signer.insecure_clone(),
            builds,
        };
        vec![
            Box::new(pool("orca:amm:A")),
            Box::new(lender(chain, signer)),
            Box::new(pool("raydium:amm:B")),
        ]
    }

    /// The group's transaction as the chain got it: each instruction's
    /// program and data.
    fn sent(chain: &MockChainClient) -> Vec<(Pubkey, Vec<u8>)> {
        let sent = chain.sent();
        assert_eq!(sent.len(), 1, "one transaction for the group");
        let message = &sent[0].message;
        assert!(
            message.address_table_lookups().is_none(),
            "no lookup tables: a legacy message"
        );
        let keys = message.static_account_keys();
        message
            .instructions()
            .iter()
            .map(|ix| (keys[usize::from(ix.program_id_index)], ix.data.clone()))
            .collect()
    }

    #[tokio::test]
    async fn a_group_is_one_transaction_laid_out_as_scope_and_klend_require() {
        let chain = Arc::new(MockChainClient::new());
        let signer = Keypair::new();
        let seats = seats(&chain, &signer, true);
        let options = TxOptions {
            cu_price: Some(7),
            ..TxOptions::default()
        };
        let (plan, seat) = build(&route(options), |v| seat_in(&seats, v), Some(options))
            .await
            .expect("the group builds");
        assert_eq!(seat.market(), "orca:amm:A", "the first line's seat sends");
        assert!(seat.send_plan(&plan).await.sent);

        let ata = token::create_ata_idempotent_for(
            &signer.pubkey(),
            &signer.pubkey(),
            &Pubkey::default(),
            &TOKEN_PROGRAM_ID,
        )
        .program_id;
        let layout = sent(&chain);
        let programs: Vec<Pubkey> = layout.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            programs,
            [
                COMPUTE_BUDGET,
                COMPUTE_BUDGET,
                scope::PROGRAM_ID,
                ata,
                klend::PROGRAM_ID,
                SWAP_PROGRAM,
                SWAP_PROGRAM,
                klend::PROGRAM_ID,
            ]
        );
        // The limit is every line's added up: two swaps, the push, the loan.
        let limit = 200_000u32 + 200_000 + 150_000 + 120_000;
        assert_eq!(layout[0].1, [&[2][..], &limit.to_le_bytes()].concat());
        assert_eq!(layout[1].1, [&[3][..], &7u64.to_le_bytes()].concat());
        assert_eq!(layout[2].1[8..], [1, 0, 0, 0, 5, 0], "entry 5 alone");
        assert_eq!(
            layout[4].1[..8],
            klend::instruction_tag("flash_borrow_reserve_liquidity")
        );
        assert_eq!(
            (layout[5].1.as_slice(), layout[6].1.as_slice()),
            (&[0][..], &[3][..])
        );
        let repay = &layout[7].1;
        assert_eq!(
            repay[..8],
            klend::instruction_tag("flash_repay_reserve_liquidity")
        );
        assert_eq!(repay[8..16], 10_000_000_000u64.to_le_bytes(), "10 SOL back");
        assert_eq!(repay[16], 4, "the repay names the borrow's index");
    }

    /// Without a priority fee the budget is the limit alone, and the borrow
    /// moves up one; the options' own limit wins over the sum.
    #[tokio::test]
    async fn the_borrow_index_follows_the_budget() {
        let chain = Arc::new(MockChainClient::new());
        let signer = Keypair::new();
        let seats = seats(&chain, &signer, true);
        let options = TxOptions {
            cu_limit: Some(900_000),
            ..TxOptions::default()
        };
        let (plan, seat) = build(&route(options), |v| seat_in(&seats, v), Some(options))
            .await
            .expect("the group builds");
        assert!(seat.send_plan(&plan).await.sent);
        let layout = sent(&chain);
        assert_eq!(layout[0].1, [&[2][..], &900_000u32.to_le_bytes()].concat());
        assert_eq!(layout[3].0, klend::PROGRAM_ID, "the borrow");
        assert_eq!(layout.last().expect("a repay").1[16], 3);

        // Options the session doesn't apply leave the sum and no price.
        let (plan, _) = build(&route(options), |v| seat_in(&seats, v), None)
            .await
            .expect("the group builds");
        assert_eq!(plan.budget_len(), 1);
        assert_eq!(plan.compute_unit_limit, 670_000);
    }

    #[tokio::test]
    async fn a_group_that_cant_be_built_whole_isnt_built() {
        let chain = Arc::new(MockChainClient::new());
        let signer = Keypair::new();
        let options = TxOptions::default();

        // A pool that won't build its line refuses the group, by line.
        let refusing = seats(&chain, &signer, false);
        let refused = build(&route(options), |v| seat_in(&refusing, v), None).await;
        assert_eq!(
            refused.err().as_deref(),
            Some("line 0: the pool moved past the limit")
        );

        let seats = seats(&chain, &signer, true);
        // A venue the session doesn't trade.
        let mut elsewhere = route(options);
        elsewhere[3].venue = Some("meteora:amm:C".into());
        let refused = build(&elsewhere, |v| seat_in(&seats, v), None).await;
        assert!(
            refused
                .err()
                .is_some_and(|e| e.starts_with("line 3: meteora:amm:C"))
        );

        // An action the venue doesn't execute.
        let mut wrong = route(options);
        wrong[0].action = Action::Flash { amount: 1.0 };
        let refused = build(&wrong, |v| seat_in(&seats, v), None).await;
        assert!(
            refused
                .err()
                .is_some_and(|e| e.starts_with("line 0: orca:amm:A doesn't execute"))
        );

        // Two flash loans.
        let mut two = route(options);
        two[2].action = Action::Flash { amount: 1.0 };
        let refused = build(&two, |v| seat_in(&seats, v), None).await;
        assert_eq!(
            refused.err().as_deref(),
            Some("a group holds at most one flash loan")
        );
        assert!(chain.sent().is_empty(), "nothing went out");
    }
}
