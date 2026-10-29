//! Transaction plans: packing, simulation, execution.
//!
//! **Providers describe; luce acts.** A protocol adapter hands back
//! [`InstructionGroup`]s: bundles that must land in the same transaction (a
//! refresh and the deposit that depends on it). [`pack`] fits groups into as
//! few transactions as the wire limit allows, never splitting a group. The
//! result is an [`ExecutionPlan`]: an ordered list of transactions, each with
//! a compute budget, all paid by one payer.
//!
//! Then **plan → preview → execute**:
//!
//! 1. [`preview`] simulates every step against current state, for a human or
//!    a strategy to inspect. Nothing is signed.
//! 2. [`execute`] re-simulates each step right before signing it. A step that
//!    would fail is never signed, and nothing after it is sent. Steps run in
//!    order because later ones usually depend on accounts earlier ones create.

use std::time::Duration;

use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_pubkey::Pubkey;
use solana_transaction::{
    Hash, Instruction, Message, Signature, Transaction, versioned::VersionedTransaction,
};

use crate::chain::{
    client::{ChainClient, SimulationOutcome, TransactionSummary},
    error::{ChainError, PlanError, SignerError},
    signer::{TransactionSigner, is_fully_signed},
};

/// Compute-unit limit requested for every transaction unless a plan says
/// otherwise. Operations with on-chain refreshes and CPIs run well above the
/// 200k default; over-requesting costs nothing without a priority fee.
pub const DEFAULT_COMPUTE_UNIT_LIMIT: u32 = 600_000;

/// Largest serialized transaction a node accepts: an IPv6 MTU minus the IP
/// and UDP headers (`1280 − 40 − 8`).
pub const MAX_TRANSACTION_SIZE: usize = 1232;

/// Instructions that must execute atomically, in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionGroup {
    /// Short description, e.g. `"wrap SOL"` or `"limit order"`.
    pub label: String,
    /// The instructions, in order.
    pub instructions: Vec<Instruction>,
}

impl InstructionGroup {
    /// Creates a group.
    pub fn new(label: impl Into<String>, instructions: Vec<Instruction>) -> Self {
        Self {
            label: label.into(),
            instructions,
        }
    }
}

/// One transaction of a plan, before blockhash and signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionPlan {
    /// Labels of the groups packed into this transaction.
    pub labels: Vec<String>,
    /// Instructions, excluding the compute-budget prefix.
    pub instructions: Vec<Instruction>,
    /// Compute-unit limit requested.
    pub compute_unit_limit: u32,
}

impl TransactionPlan {
    /// A transaction holding one group.
    pub fn from_group(group: &InstructionGroup, compute_unit_limit: u32) -> Self {
        Self {
            labels: vec![group.label.clone()],
            instructions: group.instructions.clone(),
            compute_unit_limit,
        }
    }

    fn with_group(&self, group: &InstructionGroup) -> Self {
        let mut next = self.clone();
        next.labels.push(group.label.clone());
        next.instructions.extend(group.instructions.iter().cloned());
        next
    }

    /// Human-readable label joining the packed groups.
    pub fn label(&self) -> String {
        self.labels.join(" + ")
    }

    /// Full instruction list including the compute-budget prefix.
    pub fn all_instructions(&self) -> Vec<Instruction> {
        let mut all = Vec::with_capacity(self.instructions.len() + 1);
        all.push(ComputeBudgetInstruction::set_compute_unit_limit(
            self.compute_unit_limit,
        ));
        all.extend(self.instructions.iter().cloned());
        all
    }

    /// The legacy message for `payer` at `blockhash`.
    pub fn message(&self, payer: &Pubkey, blockhash: Hash) -> Message {
        Message::new_with_blockhash(&self.all_instructions(), Some(payer), &blockhash)
    }

    /// An unsigned transaction for `payer` at `blockhash`.
    pub fn transaction(&self, payer: &Pubkey, blockhash: Hash) -> Transaction {
        Transaction::new_unsigned(self.message(payer, blockhash))
    }

    /// Serialized size in bytes once signed, independent of the blockhash.
    pub fn serialized_size(&self, payer: &Pubkey) -> usize {
        let message = self.message(payer, Hash::default());
        let signatures = usize::from(message.header.num_required_signatures);
        // compact-u16 signature count (1 byte below 128) + 64 bytes each.
        1 + 64 * signatures + message.serialize().len()
    }
}

/// An ordered list of transactions that together perform one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// Fee payer and sole signer.
    pub payer: Pubkey,
    /// Transactions in execution order.
    pub steps: Vec<TransactionPlan>,
}

impl ExecutionPlan {
    /// Number of transactions.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// `true` when there is nothing to execute.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Total number of instructions across all steps, excluding
    /// compute-budget prefixes.
    pub fn instruction_count(&self) -> usize {
        self.steps.iter().map(|s| s.instructions.len()).sum()
    }
}

/// Packs groups into transactions greedily, preserving order and never
/// splitting a group. Empty groups are dropped.
pub fn pack(
    payer: &Pubkey,
    groups: &[InstructionGroup],
    compute_unit_limit: u32,
) -> Result<ExecutionPlan, PlanError> {
    let mut steps = Vec::new();
    let mut current: Option<TransactionPlan> = None;

    for group in groups.iter().filter(|g| !g.instructions.is_empty()) {
        let candidate = match &current {
            Some(plan) => plan.with_group(group),
            None => TransactionPlan::from_group(group, compute_unit_limit),
        };
        if candidate.serialized_size(payer) <= MAX_TRANSACTION_SIZE {
            current = Some(candidate);
            continue;
        }
        if let Some(full) = current.take() {
            steps.push(full);
        }
        let alone = TransactionPlan::from_group(group, compute_unit_limit);
        let size = alone.serialized_size(payer);
        if size > MAX_TRANSACTION_SIZE {
            return Err(PlanError::GroupTooLarge {
                label: group.label.clone(),
                size,
                limit: MAX_TRANSACTION_SIZE,
            });
        }
        current = Some(alone);
    }
    if let Some(last) = current {
        steps.push(last);
    }
    if steps.is_empty() {
        return Err(PlanError::Empty);
    }
    Ok(ExecutionPlan {
        payer: *payer,
        steps,
    })
}

/// Simulation result for one step of a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPreview {
    /// The step's label.
    pub label: String,
    /// Simulation against the *current* chain state.
    pub outcome: SimulationOutcome,
    /// `true` for every step after the first: its simulation ran without the
    /// effects of earlier steps, so a failure here is advisory. Each step is
    /// re-simulated with real state right before it is signed.
    pub depends_on_previous: bool,
}

impl StepPreview {
    /// `true` if the simulation reported no error.
    pub fn succeeded(&self) -> bool {
        self.outcome.error.is_none()
    }
}

/// Simulates every step against the current chain state.
///
/// The first step's result is authoritative; later steps may fail only
/// because they depend on earlier ones. Nothing is signed or sent.
pub async fn preview(
    plan: &ExecutionPlan,
    client: &dyn ChainClient,
    watch: &[Pubkey],
) -> Result<Vec<StepPreview>, ChainError> {
    let blockhash = client.get_latest_blockhash().await?;
    let mut previews = Vec::with_capacity(plan.steps.len());
    for (index, step) in plan.steps.iter().enumerate() {
        let tx = VersionedTransaction::from(step.transaction(&plan.payer, blockhash));
        let outcome = client.simulate(&tx, watch).await?;
        previews.push(StepPreview {
            label: step.label(),
            outcome,
            depends_on_previous: index > 0,
        });
    }
    Ok(previews)
}

/// What one executed step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReceipt {
    /// The step's label.
    pub label: String,
    /// Signature of the confirmed transaction.
    pub signature: Signature,
    /// Pre-flight simulation that preceded signing.
    pub simulation: SimulationOutcome,
    /// Balance effects, when receipts were requested and the node returned
    /// the record in time.
    pub summary: Option<TransactionSummary>,
}

/// Knobs for [`execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// How many times to poll for the confirmed transaction record. Zero
    /// skips receipts entirely, saving a round trip per step.
    pub receipt_attempts: u32,
    /// Delay between polls.
    pub receipt_delay: Duration,
}

impl ExecuteOptions {
    /// Confirm each step, but do not fetch its record. For latency-sensitive
    /// callers that read state back some other way.
    pub const fn without_receipts() -> Self {
        Self {
            receipt_attempts: 0,
            receipt_delay: Duration::ZERO,
        }
    }
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            receipt_attempts: 10,
            receipt_delay: Duration::from_secs(1),
        }
    }
}

/// Executes a plan step by step: simulate, sign, send, confirm, fetch receipt.
///
/// Stops at the first failure; earlier steps stay on chain. `watch` accounts
/// are included in each simulation's post-state for reporting.
pub async fn execute(
    plan: &ExecutionPlan,
    signer: &dyn TransactionSigner,
    client: &dyn ChainClient,
    watch: &[Pubkey],
    options: ExecuteOptions,
) -> Result<Vec<ExecutionReceipt>, ChainError> {
    if signer.address() != plan.payer {
        return Err(ChainError::PayerMismatch {
            expected: plan.payer,
            actual: signer.address(),
        });
    }
    if plan.is_empty() {
        return Err(PlanError::Empty.into());
    }

    let mut receipts = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let label = step.label();
        let blockhash = client.get_latest_blockhash().await?;
        let mut tx = VersionedTransaction::from(step.transaction(&plan.payer, blockhash));

        let simulation = client.simulate(&tx, watch).await?;
        if let Some(error) = simulation.error.clone() {
            return Err(ChainError::Simulation {
                label,
                error,
                logs: simulation.logs,
            });
        }

        signer.sign_transaction(&mut tx).await?;
        if !is_fully_signed(&tx) {
            let required = usize::from(tx.message.header().num_required_signatures);
            let present = tx
                .signatures
                .iter()
                .filter(|s| **s != Signature::default())
                .count();
            return Err(SignerError::Incomplete {
                missing: required.saturating_sub(present),
                required,
            }
            .into());
        }

        let signature = client.send_and_confirm(&tx).await.map_err(|e| match e {
            ChainError::Failed {
                signature, error, ..
            } => ChainError::Failed {
                label: label.clone(),
                signature,
                error,
            },
            other => other,
        })?;
        tracing::info!(%signature, label, "transaction confirmed");

        let summary = fetch_receipt(client, &signature, options).await?;
        if let Some(error) = summary.as_ref().and_then(|s| s.error.clone()) {
            return Err(ChainError::Failed {
                label,
                signature,
                error,
            });
        }

        receipts.push(ExecutionReceipt {
            label,
            signature,
            simulation,
            summary,
        });
    }
    Ok(receipts)
}

async fn fetch_receipt(
    client: &dyn ChainClient,
    signature: &Signature,
    options: ExecuteOptions,
) -> Result<Option<TransactionSummary>, ChainError> {
    for attempt in 0..options.receipt_attempts {
        if let Some(summary) = client.get_transaction(signature).await? {
            return Ok(Some(summary));
        }
        if attempt + 1 < options.receipt_attempts && !options.receipt_delay.is_zero() {
            tokio::time::sleep(options.receipt_delay).await;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use solana_keypair::Keypair;
    use solana_signer::Signer as _;
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::TransactionError;

    use super::*;
    use crate::chain::mock::MockChainClient;

    fn transfer(from: &Pubkey) -> Instruction {
        system_instruction::transfer(from, &Pubkey::new_unique(), 1)
    }

    fn group(label: &str, payer: &Pubkey, n: usize) -> InstructionGroup {
        InstructionGroup::new(label, (0..n).map(|_| transfer(payer)).collect())
    }

    #[test]
    fn serialized_size_matches_wire_encoding_of_signed_transaction() {
        let payer = Pubkey::new_unique();
        let plan = TransactionPlan::from_group(&group("g", &payer, 3), DEFAULT_COMPUTE_UNIT_LIMIT);
        let mut tx = plan.transaction(&payer, Hash::default());
        tx.signatures = vec![Signature::default()];
        let wire = wincode::serialize(&tx).unwrap();
        assert_eq!(plan.serialized_size(&payer), wire.len());
    }

    #[test]
    fn pack_merges_small_groups_into_one_transaction() {
        let payer = Pubkey::new_unique();
        let groups = vec![
            group("a", &payer, 1),
            group("b", &payer, 2),
            group("c", &payer, 1),
        ];
        let plan = pack(&payer, &groups, DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.steps[0].labels, vec!["a", "b", "c"]);
        assert_eq!(plan.steps[0].instructions.len(), 4);
        assert_eq!(plan.steps[0].label(), "a + b + c");
        assert_eq!(plan.instruction_count(), 4);
        // Compute budget prefix is present exactly once.
        let all = plan.steps[0].all_instructions();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].program_id, solana_compute_budget_interface::id());
    }

    #[test]
    fn pack_splits_when_limit_exceeded_and_keeps_groups_atomic() {
        let payer = Pubkey::new_unique();
        // Each transfer adds a fresh account (32 bytes) plus instruction bytes;
        // 12 transfers fit, 24 do not.
        let groups = vec![
            group("a", &payer, 12),
            group("b", &payer, 12),
            group("c", &payer, 1),
        ];
        let plan = pack(&payer, &groups, DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        assert!(plan.len() >= 2, "{} steps", plan.len());
        for step in &plan.steps {
            assert!(step.serialized_size(&payer) <= MAX_TRANSACTION_SIZE);
        }
        // Groups are never split: every label appears exactly once, in order.
        let labels: Vec<&String> = plan.steps.iter().flat_map(|s| s.labels.iter()).collect();
        assert_eq!(labels, vec!["a", "b", "c"]);
    }

    #[test]
    fn pack_rejects_oversized_group_and_empty_plan() {
        let payer = Pubkey::new_unique();
        let huge = group("huge", &payer, 40);
        let err = pack(&payer, &[huge], DEFAULT_COMPUTE_UNIT_LIMIT).unwrap_err();
        assert!(matches!(err, PlanError::GroupTooLarge { label, .. } if label == "huge"));

        assert_eq!(
            pack(
                &payer,
                &[InstructionGroup::new("empty", vec![])],
                DEFAULT_COMPUTE_UNIT_LIMIT
            )
            .unwrap_err(),
            PlanError::Empty
        );
        assert_eq!(
            pack(&payer, &[], DEFAULT_COMPUTE_UNIT_LIMIT).unwrap_err(),
            PlanError::Empty
        );
    }

    fn fast_options() -> ExecuteOptions {
        ExecuteOptions {
            receipt_attempts: 2,
            receipt_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn execute_simulates_signs_sends_and_collects_receipts() {
        let signer = Keypair::new();
        let payer = signer.pubkey();
        let client = MockChainClient::new();
        let groups = vec![group("a", &payer, 12), group("b", &payer, 12)];
        let plan = pack(&payer, &groups, DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        assert_eq!(plan.len(), 2);

        client.push_simulation(SimulationOutcome {
            units_consumed: Some(1234),
            ..Default::default()
        });

        let receipts = execute(&plan, &signer, &client, &[payer], fast_options())
            .await
            .unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].label, "a");
        assert_eq!(receipts[0].simulation.units_consumed, Some(1234));
        assert!(
            receipts[0].summary.is_none(),
            "mock has no receipt registered"
        );

        let sent = client.sent();
        assert_eq!(sent.len(), 2);
        for tx in &sent {
            assert!(
                tx.verify_and_hash_message().is_ok(),
                "transaction must be fully signed"
            );
            assert_eq!(tx.message.static_account_keys()[0], payer);
        }
    }

    #[tokio::test]
    async fn execute_without_receipts_never_asks_for_them() {
        let signer = Keypair::new();
        let payer = signer.pubkey();
        let client = MockChainClient::new();
        let plan = pack(&payer, &[group("a", &payer, 1)], DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        let receipts = execute(
            &plan,
            &signer,
            &client,
            &[],
            ExecuteOptions::without_receipts(),
        )
        .await
        .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(client.receipt_requests(), 0);
    }

    #[tokio::test]
    async fn execute_stops_before_signing_when_simulation_fails() {
        let signer = Keypair::new();
        let payer = signer.pubkey();
        let client = MockChainClient::new();
        // Two large groups cannot share a transaction, so the plan has two steps.
        let plan = pack(
            &payer,
            &[group("a", &payer, 12), group("b", &payer, 12)],
            DEFAULT_COMPUTE_UNIT_LIMIT,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        client.push_simulation(SimulationOutcome::default());
        client.push_simulation(SimulationOutcome {
            error: Some(TransactionError::InsufficientFundsForFee),
            logs: vec!["Program log: nope".into()],
            ..Default::default()
        });

        let err = execute(&plan, &signer, &client, &[], fast_options())
            .await
            .unwrap_err();
        match err {
            ChainError::Simulation { label, error, logs } => {
                assert_eq!(label, plan.steps.last().unwrap().label());
                assert_eq!(error, TransactionError::InsufficientFundsForFee);
                assert_eq!(logs, vec!["Program log: nope"]);
            }
            other => panic!("unexpected {other}"),
        }
        assert_eq!(
            client.sent().len(),
            plan.len() - 1,
            "only the steps before the failure were sent"
        );
    }

    #[tokio::test]
    async fn execute_reports_on_chain_failure() {
        let signer = Keypair::new();
        let payer = signer.pubkey();
        let client = MockChainClient::new();
        let plan = pack(&payer, &[group("a", &payer, 1)], DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        client.push_send_failure(TransactionError::AccountNotFound);

        let err = execute(&plan, &signer, &client, &[], fast_options())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ChainError::Failed { label, error: TransactionError::AccountNotFound, .. } if label == "a")
        );
    }

    #[tokio::test]
    async fn execute_rejects_wrong_payer() {
        let signer = Keypair::new();
        let payer = Pubkey::new_unique();
        let client = MockChainClient::new();
        let plan = pack(&payer, &[group("a", &payer, 1)], DEFAULT_COMPUTE_UNIT_LIMIT).unwrap();
        let err = execute(&plan, &signer, &client, &[], fast_options())
            .await
            .unwrap_err();
        assert!(matches!(err, ChainError::PayerMismatch { .. }));
        assert!(client.sent().is_empty());
    }

    #[tokio::test]
    async fn preview_marks_dependent_steps() {
        let payer = Pubkey::new_unique();
        let client = MockChainClient::new();
        let plan = pack(
            &payer,
            &[group("a", &payer, 12), group("b", &payer, 12)],
            DEFAULT_COMPUTE_UNIT_LIMIT,
        )
        .unwrap();
        client.push_simulation(SimulationOutcome::default());
        client.push_simulation(SimulationOutcome {
            error: Some(TransactionError::AccountNotFound),
            ..Default::default()
        });
        let previews = preview(&plan, &client, &[]).await.unwrap();
        assert_eq!(previews.len(), 2);
        assert!(previews[0].succeeded());
        assert!(!previews[0].depends_on_previous);
        assert!(!previews[1].succeeded());
        assert!(previews[1].depends_on_previous);
        assert!(client.sent().is_empty());
    }
}
