//! Real-runtime checks for the guardless protocol. No fake AMM or mocked RPC.
use anyhow::{Context as _, Result};
use base64::{Engine, prelude::BASE64_STANDARD};
use futures::{SinkExt, StreamExt};
use mth_backrun::{
    fixture::{Market, PoolKind},
    gateway::Gateway,
    programs::*,
    sdk::Context,
    wallet::Wallet,
    world::World,
};
use serde_json::json;
use solana_signer::Signer;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MTH_BACKRUN_FIXTURE"]
async fn native_rpc_clock_wallet_inventory_and_fees() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("MTH_BACKRUN_FIXTURE")?);
    let mut market: Market = serde_json::from_slice(&std::fs::read(root.join("market.json"))?)?;
    market.hydrate();
    let world = World::restore(&root.join("snapshot.json")).await?;
    world
        .surfnet
        .cheatcodes()
        .fund_sol(&world.payer().pubkey(), 1_000_000_000_000_000)?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(200000, 1800000000000)?;
    // More than one SDK event buffer of setup traffic must not block a later
    // lossless ClockUpdate notification. No trading capital changes here.
    for amount in 1..=300 {
        world
            .send(
                vec![solana_system_interface::instruction::transfer(
                    &world.payer().pubkey(),
                    &world.payer().pubkey(),
                    amount,
                )],
                &[],
            )
            .await?;
    }
    let wallet = Wallet::prepare(&world, &market, mth_backrun::runner::REFERENCE_EXECUTOR).await?;
    let context = Context::new(0, market.clone(), &wallet, &[]);
    world.advance_slot().await?;
    world.lock_mutations()?;
    assert!(
        world
            .surfnet
            .cheatcodes()
            .fund_sol(&wallet.trader.pubkey(), 123)
            .is_err()
    );
    let initial = world.export()?;
    let value = mth_backrun::score::capital(&initial, &wallet.trader.pubkey().to_string(), &[])?;
    let gateway = Gateway::new(
        vec![context.clone()],
        vec!["test".into()],
        world.surfnet.rpc_url().into(),
        world.surfnet.ws_url().into(),
    );
    gateway.state.lock().unwrap().running = true;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let g = gateway.clone();
    let server = tokio::spawn(async move { axum::serve(listener, g.router()).await });
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{address}/ws").into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", "Bearer test".parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"slotSubscribe"})
            .to_string()
            .into(),
    ))
    .await?;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await?
        .context("missing subscription")??;
    assert!(reply.to_text()?.contains("result"));
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::CommandClock(
            None,
            surfpool_types::ClockCommand::Pause,
        ))?;
    let base = world.rpc.get_slot()?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Clock,
        ))?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::CommandClock(
            None,
            surfpool_types::ClockCommand::Resume,
        ))?;
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await?
        .context("missing notification")??;
    assert!(update.to_text()?.contains("slotNotification"));
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Manual,
        ))?;
    world.advance_slot().await?;
    assert!(world.rpc.get_slot()? > base);
    // An ordinary one-leg swap leaves token inventory. There is no cycle guard.
    let pool = market
        .pools
        .iter()
        .find(|p| p.kind == PoolKind::ConstantProduct && p.mints.contains(&0))
        .context("missing WSOL pool")?;
    let direction = pool.mints[0] == 0;
    let swap = cpmm_swap(pool, wallet.trader.pubkey(), direction, 100_000_000, 1);
    let wire = context.transaction(
        vec![swap],
        &world.rpc.get_latest_blockhash()?.to_string(),
        400_000,
        1000,
    )?;
    let client = reqwest::Client::new();
    let request = json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":[BASE64_STANDARD.encode(&wire),{"encoding":"base64","skipPreflight":true}]});
    let result: serde_json::Value = client
        .post(format!("http://{address}/rpc"))
        .bearer_auth("test")
        .json(&request)
        .send()
        .await?
        .json()
        .await?;
    assert!(result.get("error").is_none(), "{result}");
    world.advance_slot().await?;
    let mint = key(&market.tokens[pool.mints[usize::from(direction)]]);
    assert!(world.token_balance(wallet.trader.pubkey(), mint)? > 0);
    assert_eq!(
        world.rpc.get_balance(&wallet.trader.pubkey())?,
        mth_backrun::FEE_LAMPORTS - 5400
    );
    // An excessive min-out reverts, retains inventory, and pays its actual fee.
    let swap = cpmm_swap(
        pool,
        wallet.trader.pubkey(),
        direction,
        100_000_000,
        u64::MAX,
    );
    let wire = context.transaction(
        vec![swap],
        &world.rpc.get_latest_blockhash()?.to_string(),
        400_000,
        1000,
    )?;
    let tx = wincode::deserialize(&wire)?;
    let receipt = world.execute(&tx).await?;
    assert!(!receipt["meta"]["err"].is_null());
    assert_eq!(receipt["meta"]["fee"], 5400);
    let final_value =
        mth_backrun::score::capital(&world.export()?, &wallet.trader.pubkey().to_string(), &[])?;
    assert_eq!(i128::from(final_value) - i128::from(value), -100_010_800);
    // Closing a setup-funded empty account only loses the normal tx fee.
    let empty = wallet.token_accounts[market
        .tokens
        .iter()
        .position(|m| m != &mint.to_string() && m != mth_backrun::WSOL)
        .unwrap()];
    let close = spl_token_interface::instruction::close_account(
        &key(TOKEN),
        &empty,
        &wallet.trader.pubkey(),
        &wallet.trader.pubkey(),
        &[],
    )?;
    let wire = context.transaction(
        vec![close],
        &world.rpc.get_latest_blockhash()?.to_string(),
        400_000,
        0,
    )?;
    assert!(world.execute(&wincode::deserialize(&wire)?).await?["meta"]["err"].is_null());
    let after_close =
        mth_backrun::score::capital(&world.export()?, &wallet.trader.pubkey().to_string(), &[])?;
    assert_eq!(after_close, final_value - 5000);
    // A later higher fee does not move ahead of an already executed transfer.
    let hash = world.rpc.get_latest_blockhash()?.to_string();
    let transfer = |amount| {
        solana_system_interface::instruction::transfer(
            &wallet.trader.pubkey(),
            &world.payer().pubkey(),
            amount,
        )
    };
    let low: solana_transaction::versioned::VersionedTransaction =
        wincode::deserialize(&context.transaction(vec![transfer(1)], &hash, 400_000, 0)?)?;
    let high: solana_transaction::versioned::VersionedTransaction =
        wincode::deserialize(&context.transaction(vec![transfer(2)], &hash, 400_000, 10000)?)?;
    let a = world.execute(&low).await?;
    let b = world.execute(&high).await?;
    assert!(a["meta"]["err"].is_null() && b["meta"]["err"].is_null());
    assert_eq!(
        mth_backrun::score::priority_fee(&high, b["meta"]["fee"].as_u64().unwrap())?,
        4000
    );
    let low_final = a["meta"]["postBalances"][0].as_u64().unwrap();
    assert_eq!(b["meta"]["preBalances"][0], low_final);
    // Standard RPC mutation remains accepted; Surfpool-only signature bypass is denied.
    let mut forbidden = request;
    forbidden["params"][1]["skipSigVerify"] = json!(true);
    let response: serde_json::Value = client
        .post(format!("http://{address}/rpc"))
        .bearer_auth("test")
        .json(&forbidden)
        .send()
        .await?
        .json()
        .await?;
    assert!(response.get("error").is_some());
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MTH_BACKRUN_FIXTURE"]
async fn native_version_one_receipt_collection() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("MTH_BACKRUN_FIXTURE")?);
    let world = World::restore(&root.join("snapshot.json")).await?;
    world
        .surfnet
        .cheatcodes()
        .fund_sol(&world.payer().pubkey(), 100_000_000)?;
    let recipient = solana_keypair::Keypair::new().pubkey();
    let message = solana_message::v1::Message::try_compile_with_config(
        &world.payer().pubkey(),
        &[solana_system_interface::instruction::transfer(
            &world.payer().pubkey(),
            &recipient,
            1_000_000,
        )],
        world.rpc.get_latest_blockhash()?,
        solana_message::v1::TransactionConfig::empty()
            .with_compute_unit_limit(200_000)
            .with_loaded_accounts_data_size_limit(1_000_000)
            .with_priority_fee(10000),
    )?;
    let tx = solana_transaction::versioned::VersionedTransaction::try_new(
        solana_message::VersionedMessage::V1(message),
        &[world.payer()],
    )?;
    let receipt = world.execute(&tx).await?;
    assert!(
        !receipt.is_null() && receipt["meta"]["err"].is_null(),
        "{receipt}"
    );
    world.advance_slot().await?;
    let block:serde_json::Value=world.rpc.send(solana_rpc_client_api::request::RpcRequest::GetBlock,json!([receipt["slot"],{"encoding":"base64","commitment":"processed","maxSupportedTransactionVersion":1}]))?;
    assert!(
        block["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["version"] == 1)
    );
    assert_eq!(
        mth_backrun::score::priority_fee(&tx, receipt["meta"]["fee"].as_u64().unwrap())?,
        10000
    );
    Ok(())
}
