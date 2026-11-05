//! `mtx mcp`: an MCP server (stdio) that guides an author, and the agent
//! helping them, through creating a task. A thin layer over
//! [`crate::authoring`]: it asks, scaffolds, and reports where the task stands.
//!
//! `task_new` interviews the author with elicitation forms when the client
//! supports them (Claude Code does): basics, the skill, the mandate. Without
//! elicitation it returns the questions still open, for the agent to ask.

use std::path::PathBuf;

use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    service::{ElicitationError, ElicitationMode},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::authoring::{self, Base, TaskSpec};

pub async fn serve(root: PathBuf) -> anyhow::Result<()> {
    let service = Server::new(root).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
struct Server {
    root: PathBuf,
    #[allow(dead_code, reason = "read by the tool_handler macro")]
    tool_router: ToolRouter<Server>,
}

/// The interview, as three short forms: elicitation takes flat objects.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Basics {
    #[schemars(
        description = "Task id: venue first, words joined by hyphens, as in phoenix-funding-squeeze"
    )]
    name: String,
    #[schemars(description = "Short title, as in Funding squeeze")]
    title: String,
    #[schemars(description = "One sentence: what the agent's bot does, on which market")]
    summary: String,
    // Forms take a plain list of strings, not a described enum.
    #[schemars(
        description = "Start from: shocks (the market moves along a scripted path; from phoenix-flash-crash) or making (the grader runs counterparties the bot trades with; from phoenix-market-making)",
        extend("enum" = ["shocks", "making"])
    )]
    base: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Skill {
    #[schemars(description = "Complete the sentence: a good bot profits here because it ___")]
    thesis: String,
    #[schemars(description = "What wins: the expert's method")]
    wins: String,
    #[schemars(description = "The habits that lose, and in which regime they fail")]
    loses: String,
    #[schemars(description = "How scenarios vary: the regimes and their odds")]
    regimes: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Mandate {
    #[schemars(description = "Position cap in SPCX (70 is about 1x the 10,000 USDC collateral)")]
    max_position: f64,
    #[schemars(description = "Loss limit per session in USD (1000 is 10% of collateral)")]
    max_drawdown_usd: f64,
    #[schemars(description = "Hidden sessions per grade (72 by default)")]
    hidden_sessions: u32,
}

rmcp::elicit_safe!(Basics, Skill, Mandate);

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskParams {
    #[schemars(
        description = "The task's id, a directory in bench/tasks, as in phoenix-market-making. task_list lists them."
    )]
    task: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GuideParams {
    #[schemars(
        description = "The step: design, family, reference, anchor, practice, instruction, readme, registry, lint or verify (the ids task_status returns)."
    )]
    step: String,
}

fn ok(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
    )]))
}

/// A step list with the one to do now on top.
fn report(task: &str, steps: Vec<authoring::Step>) -> Value {
    let next = authoring::next(&steps).map(|s| json!({ "id": s.id, "title": s.title, "do": s.next, "guide": format!("task_guide step={}", s.id) }));
    let done = steps.iter().filter(|s| s.state == "done").count();
    json!({
        "task": task,
        "progress": format!("{done} of {} steps done", steps.len()),
        "next": next.unwrap_or_else(|| json!({ "id": "verify", "do": "Every file-level step is done: run the checks on the image and real agents (task_guide step=verify)." })),
        "steps": steps,
    })
}

#[tool_router]
impl Server {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a new trading task, guided. Interviews the author with forms (elicitation) for whatever the arguments leave out: the task's id, title and one-line summary, which existing task to start from, the skill thesis, what wins and what loses, how scenarios vary, the position cap and loss limit. Then scaffolds bench/tasks/<name> and its reference bot, and returns the task's checklist with the next step. Without elicitation support it returns `questions`: ask the author those, then call again with every answer."
    )]
    async fn task_new(
        &self,
        peer: Peer<RoleServer>,
        Parameters(mut spec): Parameters<TaskSpec>,
    ) -> Result<CallToolResult, McpError> {
        let forms = peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form);
        if forms {
            let wants = |spec: &TaskSpec, fields: &[&str]| {
                spec.missing().iter().any(|m| fields.contains(m))
            };
            if wants(&spec, &["name", "title", "summary", "base"]) {
                match peer
                    .elicit::<Basics>("New trading task (1/3): what is it?")
                    .await
                {
                    Ok(Some(b)) => {
                        spec.name = b.name;
                        spec.title = b.title;
                        spec.summary = b.summary;
                        spec.base = Some(if b.base == "making" {
                            Base::Making
                        } else {
                            Base::Shocks
                        });
                    }
                    Ok(None)
                    | Err(ElicitationError::UserDeclined)
                    | Err(ElicitationError::UserCancelled) => {
                        return ok(
                            json!({ "cancelled": true, "message": "The author stopped the interview; nothing was created." }),
                        );
                    }
                    Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
                }
            }
            if wants(&spec, &["thesis", "wins", "loses", "regimes"]) {
                match peer.elicit::<Skill>("New trading task (2/3): the skill it measures. If you can't complete the thesis, the task measures noise.").await {
                    Ok(Some(s)) => {
                        spec.thesis = s.thesis;
                        spec.wins = s.wins;
                        spec.loses = s.loses;
                        spec.regimes = s.regimes;
                    }
                    Ok(None) | Err(ElicitationError::UserDeclined) | Err(ElicitationError::UserCancelled) => {
                        return ok(json!({ "cancelled": true, "message": "The author stopped the interview; nothing was created." }));
                    }
                    Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
                }
            }
            if wants(&spec, &["max_position", "max_drawdown_usd"]) {
                match peer.elicit::<Mandate>("New trading task (3/3): the mandate. Size the reference to the cap, and set the loss limit so it stays inside.").await {
                    Ok(Some(m)) => {
                        spec.max_position = Some(m.max_position);
                        spec.max_drawdown_usd = Some(m.max_drawdown_usd);
                        spec.hidden_sessions = Some(m.hidden_sessions);
                    }
                    Ok(None) | Err(ElicitationError::UserDeclined) | Err(ElicitationError::UserCancelled) => {
                        return ok(json!({ "cancelled": true, "message": "The author stopped the interview; nothing was created." }));
                    }
                    Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
                }
            }
        }
        let problems = spec.problems(&self.root);
        if !problems.is_empty() {
            let questions: Vec<Value> = spec
                .missing()
                .iter()
                .map(|f| json!({ "field": f, "ask": authoring::QUESTIONS.iter().find(|(q, _)| q == f).map(|(_, a)| *a) }))
                .collect();
            return ok(json!({
                "created": false,
                "problems": problems,
                "questions": questions,
                "how": "Ask the author these (one at a time is fine), then call task_new again with every answer, plus the ones you already had.",
            }));
        }
        match authoring::scaffold(&self.root, &spec) {
            Ok(out) => {
                let steps = authoring::status(&self.root, spec.name.trim()).unwrap_or_default();
                ok(
                    json!({ "created": true, "written": out["written"], "status": report(spec.name.trim(), steps) }),
                )
            }
            Err(e) => ok(json!({ "created": false, "problems": [e.to_string()] })),
        }
    }

    #[tool(
        description = "Where a task stands on its way to publication: each step (design record, scenario family, reference bot, image anchor, practice set, instruction, README, CI and dataset, lint, image checks) as done, todo or manual, read from the task's files, with what to do next. Call it after every step."
    )]
    async fn task_status(
        &self,
        Parameters(p): Parameters<TaskParams>,
    ) -> Result<CallToolResult, McpError> {
        match authoring::status(&self.root, p.task.trim()) {
            Ok(steps) => ok(report(p.task.trim(), steps)),
            Err(e) => ok(json!({ "error": e.to_string() })),
        }
    }

    #[tool(
        description = "The procedure for one step of making a task, from the trading-task-design skill, with where it goes deeper. Steps: design, family, reference, anchor, practice, instruction, readme, registry, lint, verify."
    )]
    async fn task_guide(
        &self,
        Parameters(p): Parameters<GuideParams>,
    ) -> Result<CallToolResult, McpError> {
        match authoring::guide(p.step.trim()) {
            Some(text) => ok(json!({ "step": p.step.trim(), "guide": text })),
            None => ok(
                json!({ "error": format!("no step `{}`. Steps: {}.", p.step.trim(), authoring::STEPS.join(", ")) }),
            ),
        }
    }

    #[tool(description = "The repository's tasks, with their versions and scenario families.")]
    async fn task_list(&self) -> Result<CallToolResult, McpError> {
        ok(json!({ "tasks": authoring::list(&self.root) }))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mtx", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Guides the making of a machine-trading task. Start with task_new (it interviews the author), then call task_status after every step and follow `next`; task_guide explains a step. The trading-task-design skill has the reasoning behind each.",
            )
    }
}
