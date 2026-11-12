//! Token-2022 and v0 transactions end to end, on an offline surfnet behind
//! the bot's guard: a mint with a transfer fee funded the way spot setup
//! funds a wallet, a transfer through the guard whose fee the kit predicts,
//! and a v0 transaction whose accounts come from a lookup table.

use mtx_grader::guard::Guard;
use mtx_kit::{
    chain::{
        ChainClient, ExecuteOptions, InstructionGroup, RpcChainClient, execute, lookup, pack,
        pack_with_lookup_tables, token,
    },
    token::{TOKEN_2022_PROGRAM_ID, TokenAccountInfo},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_token_2022_interface::extension::{ExtensionType, transfer_fee};
use surfpool_sdk::Surfnet;

const DECIMALS: u8 = 6;
const FEE_BPS: u16 = 100;
const MAX_FEE: u64 = 1_000_000;

/// A Token-2022 mint with a 1% transfer fee, created by ordinary
/// transactions.
fn create_mint(surfnet: &Surfnet, payer: &Keypair) -> Pubkey {
    let rpc = surfnet.rpc_client();
    let mint = Keypair::new();
    let space =
        ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(&[
            ExtensionType::TransferFeeConfig,
        ])
        .unwrap();
    let rent = rpc.get_minimum_balance_for_rent_exemption(space).unwrap();
    let instructions = vec![
        solana_system_interface::instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
            space as u64,
            &TOKEN_2022_PROGRAM_ID,
        ),
        transfer_fee::instruction::initialize_transfer_fee_config(
            &TOKEN_2022_PROGRAM_ID,
            &mint.pubkey(),
            None,
            None,
            FEE_BPS,
            MAX_FEE,
        )
        .unwrap(),
        spl_token_2022_interface::instruction::initialize_mint2(
            &TOKEN_2022_PROGRAM_ID,
            &mint.pubkey(),
            &payer.pubkey(),
            None,
            DECIMALS,
        )
        .unwrap(),
    ];
    let blockhash = rpc.get_latest_blockhash().unwrap();
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer, &mint],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&tx).unwrap();
    mint.pubkey()
}

async fn token_info(chain: &dyn ChainClient, account: &Pubkey) -> TokenAccountInfo {
    let account = chain.get_account(account).await.unwrap().unwrap();
    TokenAccountInfo::decode(&account.owner, &account.data).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn token_2022_and_v0_transactions_through_the_guard() {
    let surfnet = Surfnet::builder().offline(true).start().await.unwrap();
    let owner = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&owner.pubkey(), 10_000_000_000)
        .unwrap();
    let mint = create_mint(&surfnet, &owner);

    // Setup funds the wallet under the mint's own program.
    let funded = mtx_grader::spot::fund_mint(&surfnet, &owner.pubkey(), &mint, 5_000_000).unwrap();
    assert_eq!(funded.program, TOKEN_2022_PROGRAM_ID);
    assert_eq!(funded.decimals, DECIMALS);
    assert!(funded.transfer_fee.is_some());
    let source = token::ata_for(&owner.pubkey(), &mint, &TOKEN_2022_PROGRAM_ID);

    // The bot's side: everything through the guard, in its turn.
    let guard = Guard::start(surfnet.rpc_url()).await.unwrap();
    guard.set_turn(true);
    let chain = RpcChainClient::new(guard.url());
    let read = token::read_mint(&chain, &mint).await.unwrap();
    assert_eq!(read, funded);
    let held = token::token_account(&chain.get_account(&source).await.unwrap().unwrap()).unwrap();
    assert_eq!(held.amount, 5_000_000);

    let recipient = Pubkey::new_unique();
    let destination = token::ata_for(&recipient, &mint, &TOKEN_2022_PROGRAM_ID);
    let sent = 250_000;
    let group = InstructionGroup::new(
        "token-2022 transfer",
        vec![
            token::create_ata_idempotent_for(
                &owner.pubkey(),
                &recipient,
                &mint,
                &TOKEN_2022_PROGRAM_ID,
            ),
            token::transfer_checked_for(
                &TOKEN_2022_PROGRAM_ID,
                &source,
                &mint,
                &destination,
                &owner.pubkey(),
                sent,
                DECIMALS,
            ),
        ],
    );
    let plan = pack(&owner.pubkey(), &[group], 200_000).unwrap();
    execute(
        &plan,
        &owner,
        &chain,
        &[],
        ExecuteOptions::without_receipts(),
    )
    .await
    .unwrap();
    let epoch = surfnet.rpc_client().get_epoch_info().unwrap().epoch;
    let fee = read.transfer_fee(sent, epoch);
    assert_eq!(fee, 2_500);
    let received = token_info(&chain, &destination).await;
    assert_eq!(received.amount, read.received(sent, epoch));
    assert_eq!(received.withheld, fee);
    assert_eq!(token_info(&chain, &source).await.amount, 5_000_000 - sent);

    // A lookup table with the transfer's accounts, made during setup.
    let rpc = surfnet.rpc_client();
    let recent = rpc.get_slot().unwrap();
    let (create, table) =
        lookup::create(&owner.pubkey(), &owner.pubkey(), recent.saturating_sub(1));
    let extend = lookup::extend(
        &table,
        &owner.pubkey(),
        &owner.pubkey(),
        vec![source, destination, mint, TOKEN_2022_PROGRAM_ID],
    );
    let setup = pack(
        &owner.pubkey(),
        &[InstructionGroup::new("lookup table", vec![create, extend])],
        200_000,
    )
    .unwrap();
    execute(
        &setup,
        &owner,
        &chain,
        &[],
        ExecuteOptions::without_receipts(),
    )
    .await
    .unwrap();
    // A table serves its addresses from the slot after it was extended.
    let extended_at = rpc.get_slot().unwrap();
    while rpc.get_slot().unwrap() <= extended_at + 1 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let looked_up = lookup::lookup_table(&chain, &table).await.unwrap();
    assert_eq!(looked_up.addresses.len(), 4);

    let again = InstructionGroup::new(
        "v0 transfer",
        vec![token::transfer_checked_for(
            &TOKEN_2022_PROGRAM_ID,
            &source,
            &mint,
            &destination,
            &owner.pubkey(),
            sent,
            DECIMALS,
        )],
    );
    let v0 = pack_with_lookup_tables(&owner.pubkey(), &[again], 200_000, &[looked_up]).unwrap();
    let receipts = execute(&v0, &owner, &chain, &[], ExecuteOptions::without_receipts())
        .await
        .unwrap();
    let received = token_info(&chain, &destination).await;
    assert_eq!(received.amount, 2 * read.received(sent, epoch));

    // The guard forwarded it and logged it, and the chain ran it as v0.
    let log = guard.call_log();
    let signature = receipts[0].signature.to_string();
    assert!(
        log.sends
            .iter()
            .any(|s| s.signature.as_deref() == Some(signature.as_str()))
    );
    assert!(log.refused.is_empty());
    let found = mtx_grader::chain::call(
        &rpc,
        "getTransaction",
        serde_json::json!([signature, { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]),
    )
    .unwrap();
    assert_eq!(found["version"], 0);
    assert_eq!(
        found["meta"]["loadedAddresses"]["writable"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}
