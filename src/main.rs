use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use sousdev::{
    workflows::{
        cron_runner::CronRunner,
        executor::{resolve_system_prompt, ExecutorOptions, WorkflowExecutor},
        stores::{WorkflowResult, RunStore},
    },
    providers::resolve_provider,
    tui::events::TuiEventSender,
    techniques::{
        critique_loop::{run_critique_loop, CritiqueLoopOptions},
        multi_agent_debate::{run_multi_agent_debate, Options as MultiAgentDebateOptions},
        plan_and_solve::{run_plan_and_solve, Options as PlanAndSolveOptions},
        react::{run_react, Options as ReactOptions},
        reflexion::{run_reflexion, Options as ReflexionOptions},
        self_consistency::{run_self_consistency, Options as SelfConsistencyOptions},
        skeleton_of_thought::{run_skeleton_of_thought, Options as SkeletonOfThoughtOptions},
        tree_of_thoughts::{run_tree_of_thoughts, Options as TreeOfThoughtsOptions, Strategy},
    },
    tools::registry::ToolRegistry,
    types::technique::RunResult,
    utils::{config_loader::load_config, logger::init_logger},
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "sousdev",
    about = "🍳 SousDev — Prep, review, and plate your PRs automatically.",
    version
)]
struct Cli {
    #[arg(
        short,
        long,
        help = "Path to config.toml (auto-discovered if omitted)"
    )]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List all configured workflows.
    List,

    /// Run a workflow immediately (ignores cron schedule).
    Workflow {
        name: String,
        #[arg(long, help = "Skip workspace cloning, run in CWD")]
        no_workspace: bool,
    },

    /// Start the cron daemon for all configured workflows.
    Start {
        #[arg(long, help = "Skip workspace cloning for all workflows")]
        no_workspace: bool,
    },

    /// Show recent workflow run history.
    Status {
        name: Option<String>,
        #[arg(
            short,
            long,
            default_value = "20",
            help = "Number of recent runs to show"
        )]
        limit: usize,
    },

    /// Show full trajectory for a specific workflow run.
    Logs { name: String, run_id: String },

    /// Run a technique directly against a task.
    Run(Box<RunArgs>),

    /// List all available standalone techniques.
    Techniques,

    /// Show details for a specific technique.
    Technique { name: String },
}

/// Arguments for the `run` subcommand, boxed to reduce enum size.
#[derive(clap::Args)]
struct RunArgs {
    technique: String,
    #[arg(short, long)]
    task: String,
    #[arg(long)]
    max_iterations: Option<usize>,
    #[arg(long)]
    max_trials: Option<usize>,
    #[arg(long)]
    samples: Option<usize>,
    #[arg(long)]
    temperature: Option<f64>,
    #[arg(long)]
    branching: Option<usize>,
    #[arg(long)]
    strategy: Option<String>,
    #[arg(long)]
    max_depth: Option<usize>,
    #[arg(long)]
    max_rounds: Option<usize>,
    #[arg(long)]
    num_agents: Option<usize>,
    #[arg(long)]
    rounds: Option<usize>,
    #[arg(long)]
    aggregation: Option<String>,
    #[arg(long)]
    max_points: Option<usize>,
    #[arg(long)]
    no_parallel: bool,
}

// ---------------------------------------------------------------------------
// Technique registry
// ---------------------------------------------------------------------------

const TECHNIQUES: &[(&str, &str, &str)] = &[
    (
        "react",
        "Think → Act → Observe loop (ReAct).",
        "Yao et al., 2022 (arXiv:2210.03629)",
    ),
    (
        "reflexion",
        "Self-reflection loop; retries with written lessons-learned.",
        "Shinn et al., 2023 (arXiv:2303.11366)",
    ),
    (
        "tree-of-thoughts",
        "BFS/DFS search over candidate reasoning steps.",
        "Yao et al., 2023 (arXiv:2305.10601)",
    ),
    (
        "self-consistency",
        "Sample N chains-of-thought, return majority-vote answer.",
        "Wang et al., 2022 (arXiv:2203.11171)",
    ),
    (
        "critique-loop",
        "Generate → Critique → Revise (LLM-as-Judge).",
        "Bai et al., 2022 + Madaan et al., 2023",
    ),
    (
        "plan-and-solve",
        "Explicit plan first, then step-by-step execution (PS+).",
        "Wang et al., ACL 2023 (arXiv:2305.04091)",
    ),
    (
        "skeleton-of-thought",
        "Outline first, then expand all points in parallel.",
        "Ning et al., 2023 (arXiv:2307.15337)",
    ),
    (
        "multi-agent-debate",
        "N agents debate across rounds, judge synthesises.",
        "Du et al., 2023 (arXiv:2305.14325)",
    ),
];

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (best-effort — missing file is fine).
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Technique info commands need no config.
    match &cli.command {
        Some(Commands::Techniques) => {
            println!("\nAvailable techniques:\n");
            for (name, desc, _paper) in TECHNIQUES {
                println!("  {:<26} {}", name, desc);
            }
            println!();
            return Ok(());
        }

        Some(Commands::Technique { name }) => {
            if let Some((_, desc, paper)) = TECHNIQUES.iter().find(|(n, _, _)| *n == name.as_str())
            {
                println!("\nTechnique: {}", name);
                println!("Description: {}", desc);
                println!("Paper: {}", paper);
                println!();
            } else {
                eprintln!(
                    "Unknown technique: \"{}\". Run \"sousdev techniques\" to see available techniques.",
                    name
                );
                std::process::exit(1);
            }
            return Ok(());
        }

        _ => {}
    }

    // Load config for all remaining commands.
    let (harness_config, harness_root) =
        load_config(cli.config.as_deref())
            .await
            .map_err(|e| { eprintln!("Failed to load config:\n{}", e); e })?;

    // Only initialise the tracing-to-stdout logger for non-TUI commands.
    // The TUI installs a no-op subscriber and receives log data via its
    // event channel instead.
    if cli.command.is_some() {
        init_logger(
            harness_config
                .logging
                .as_ref()
                .and_then(|l| l.level.as_deref())
                .unwrap_or("info"),
            harness_config
                .logging
                .as_ref()
                .and_then(|l| l.pretty)
                .unwrap_or(false),
        );
    }

    match cli.command {
        // ── TUI (default — no subcommand) ─────────────────────────────────────
        None => {
            sousdev::tui::run(harness_config, false).await?;
        }

        // ── List ──────────────────────────────────────────────────────────────
        Some(Commands::List) => {
            let workflows = &harness_config.workflows;
            if workflows.is_empty() {
                println!(
                    "\nNo workflows configured. Add a `[[workflows]]` section to config.toml.\n"
                );
                return Ok(());
            }
            println!("\nConfigured workflows ({}):\n", workflows.len());
            for p in workflows {
                println!("  {:<30} schedule: {}", p.name, p.schedule);
                println!("  {:<30} technique: {}", "", p.agent_loop.technique);
                println!();
            }
        }

        // ── Workflow ──────────────────────────────────────────────────────────
        Some(Commands::Workflow { name, no_workspace }) => {
            let workflow = harness_config
                .workflows
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(|| {
                    let names: Vec<_> = harness_config
                        .workflows
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect();
                    anyhow::anyhow!(
                        "Workflow \"{}\" not found. Available: {}",
                        name,
                        names.join(", ")
                    )
                })?
                .clone();

            let provider = resolve_provider(&harness_config)?;
            let store = Arc::new(RunStore::new(&harness_root));

            println!("\nRunning workflow \"{}\"…\n", name);

            let executor = WorkflowExecutor::new(
                workflow,
                ExecutorOptions {
                    provider,
                    registry: Arc::new(ToolRegistry::new()),
                    store,
                    no_workspace,
                    target_repo: harness_config.target_repo.clone(),
                    git_method: harness_config.git_method.clone(),
                    harness_root: Some(harness_root.clone()),
                    prompts: harness_config.prompts.clone(),
                    system_prompt: resolve_system_prompt(&harness_config, &harness_root),
                    tui_tx: TuiEventSender::noop(),
                },
            );

            let result = executor.run().await?;
            print_workflow_result(&result);
            std::process::exit(if result.success { 0 } else { 1 });
        }

        // ── Start ─────────────────────────────────────────────────────────────
        Some(Commands::Start { no_workspace }) => {
            let runner = CronRunner::new(harness_config, no_workspace);
            runner.start().await?;
        }

        // ── Status ────────────────────────────────────────────────────────────
        Some(Commands::Status { name, limit }) => {
            let store = RunStore::new(&harness_root);
            let history = store.get_history(name.as_deref(), limit).await?;
            if history.is_empty() {
                let qualifier = name
                    .as_deref()
                    .map(|n| format!(" for \"{}\"", n))
                    .unwrap_or_default();
                println!("\nNo runs found{}. Run a workflow first.\n", qualifier);
                return Ok(());
            }
            println!("\nWorkflow run history ({} runs):\n", history.len());
            println!(
                "{:<28} {:<10} {:<22} {:<10} PR",
                "WORKFLOW", "RUN ID", "STARTED", "STATUS"
            );
            println!("{}", "─".repeat(90));
            for r in history.iter().rev() {
                let status = if r.skipped {
                    "skipped"
                } else if r.success {
                    "success"
                } else {
                    "failed "
                };
                let short_id = &r.run_id[..r.run_id.len().min(8)];
                let started = &r.started_at[..r.started_at.len().min(19)];
                let pr = r.pr_url.as_deref().unwrap_or("—");
                println!(
                    "{:<28} {:<10} {:<22} {:<10} {}",
                    r.workflow_name, short_id, started, status, pr
                );
            }
            println!();
        }

        // ── Logs ──────────────────────────────────────────────────────────────
        Some(Commands::Logs { name, run_id }) => {
            let store = RunStore::new(&harness_root);
            let history = store.get_history(Some(&name), 200).await?;
            let run = history
                .iter()
                .find(|r| r.run_id.starts_with(&run_id))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No run found for workflow \"{}\" with ID starting \"{}\"",
                        name,
                        run_id
                    )
                })?;

            println!("\nWorkflow: {}", run.workflow_name);
            println!("Run ID:   {}", run.run_id);
            println!("Started:  {}", run.started_at);
            println!(
                "Status:   {}",
                if run.skipped {
                    "skipped"
                } else if run.success {
                    "success"
                } else {
                    "failed"
                }
            );
            if let Some(ref url) = run.pr_url {
                println!("PR:       {}", url);
            }
            if let Some(ref err) = run.error {
                println!("Error:    {}", err);
            }
            println!();

            if run.trajectory.is_empty() {
                println!("No trajectory recorded for this run.\n");
            } else {
                println!("Trajectory:\n");
                for step in &run.trajectory {
                    let ts = &step.timestamp[..step.timestamp.len().min(19)];
                    println!("[{}] {:?} [{}]", ts, step.step_type, step.index);
                    let content = &step.content[..step.content.len().min(500)];
                    println!("{}\n", content);
                }
            }
        }

        // ── Run ───────────────────────────────────────────────────────────────
        Some(Commands::Run(args)) => {
            let RunArgs {
                technique,
                task,
                max_iterations,
                max_trials,
                samples,
                temperature,
                branching,
                strategy,
                max_depth,
                max_rounds,
                num_agents,
                rounds,
                aggregation,
                max_points,
                no_parallel,
            } = *args;
            let provider = resolve_provider(&harness_config)?;
            let registry = Arc::new(ToolRegistry::new());

            let result: RunResult = match technique.as_str() {
                "react" => {
                    run_react(ReactOptions {
                        task,
                        provider,
                        registry: Some(registry),
                        system_prompt: None,
                        max_iterations: max_iterations.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.react.as_ref()?.max_iterations)
                        }),
                        harness_root: Some(harness_root.clone()),
                    })
                    .await?
                }

                "reflexion" => {
                    run_reflexion(ReflexionOptions {
                        task,
                        provider,
                        registry: Some(registry),
                        system_prompt: None,
                        reflect_prompt: None,
                        max_trials: max_trials.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.reflexion.as_ref()?.max_trials)
                        }),
                        memory_window: harness_config
                            .techniques
                            .as_ref()
                            .and_then(|t| t.reflexion.as_ref()?.memory_window),
                        max_inner_iterations: None,
                        harness_root: Some(harness_root.clone()),
                    })
                    .await?
                }

                "tree-of-thoughts" => {
                    let strategy_val = strategy
                        .as_deref()
                        .or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.tree_of_thoughts.as_ref())
                                .and_then(|t| t.strategy.as_deref())
                        })
                        .map(|s| match s.to_lowercase().as_str() {
                            "dfs" => Strategy::Dfs,
                            _ => Strategy::Bfs,
                        });

                    run_tree_of_thoughts(TreeOfThoughtsOptions {
                        task,
                        provider,
                        branching: branching.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.tree_of_thoughts.as_ref()?.branching)
                        }),
                        strategy: strategy_val,
                        max_depth: max_depth.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.tree_of_thoughts.as_ref()?.max_depth)
                        }),
                        score_threshold: harness_config
                            .techniques
                            .as_ref()
                            .and_then(|t| t.tree_of_thoughts.as_ref()?.score_threshold),
                    })
                    .await?
                }

                "self-consistency" => {
                    run_self_consistency(SelfConsistencyOptions {
                        task,
                        provider,
                        samples: samples.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.self_consistency.as_ref()?.samples)
                        }),
                        temperature: temperature.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.self_consistency.as_ref()?.temperature)
                        }),
                    })
                    .await?
                }

                "critique-loop" => {
                    let cl_result = run_critique_loop(CritiqueLoopOptions {
                        task: task.clone(),
                        provider,
                        max_rounds: max_rounds.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.critique_loop.as_ref()?.max_rounds)
                        })
                        .unwrap_or(3),
                        criteria: harness_config
                            .techniques
                            .as_ref()
                            .and_then(|t| t.critique_loop.as_ref()?.criteria.clone())
                            .unwrap_or_default(),
                        satisfaction_threshold: 7.0,
                    })
                    .await?;

                    // Convert CritiqueLoopResult → RunResult for uniform display.
                    let llm_calls = cl_result.rounds.len() * 2; // gen + critique per round
                    RunResult::success(
                        "critique-loop",
                        cl_result.answer,
                        vec![],
                        llm_calls,
                        0,
                    )
                }

                "plan-and-solve" => {
                    run_plan_and_solve(PlanAndSolveOptions {
                        task,
                        provider,
                        registry: Some(registry),
                        detailed_plan: harness_config
                            .techniques
                            .as_ref()
                            .and_then(|t| t.plan_and_solve.as_ref()?.detailed_plan),
                        max_steps: None,
                    })
                    .await?
                }

                "skeleton-of-thought" => {
                    run_skeleton_of_thought(SkeletonOfThoughtOptions {
                        task,
                        provider,
                        max_points: max_points.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.skeleton_of_thought.as_ref()?.max_points)
                        }),
                        parallel_expansion: if no_parallel {
                            Some(false)
                        } else {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.skeleton_of_thought.as_ref()?.parallel_expansion)
                        },
                    })
                    .await?
                }

                "multi-agent-debate" => {
                    run_multi_agent_debate(MultiAgentDebateOptions {
                        task,
                        provider,
                        num_agents: num_agents.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.multi_agent_debate.as_ref()?.num_agents)
                        }),
                        rounds: rounds.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.multi_agent_debate.as_ref()?.rounds)
                        }),
                        aggregation: aggregation.or_else(|| {
                            harness_config
                                .techniques
                                .as_ref()
                                .and_then(|t| t.multi_agent_debate.as_ref()?.aggregation.clone())
                        }),
                    })
                    .await?
                }

                other => {
                    eprintln!(
                        "Unknown technique: \"{}\". Run \"sousdev techniques\" to see available.",
                        other
                    );
                    std::process::exit(1);
                }
            };

            print_technique_result(&technique, &result);
        }

        // Already handled above; unreachable here.
        Some(Commands::Techniques) | Some(Commands::Technique { .. }) => unreachable!(),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn print_workflow_result(result: &WorkflowResult) {
    let sep = "─".repeat(60);
    let icon = if result.skipped {
        "⏭"
    } else if result.success {
        "✓"
    } else {
        "✗"
    };
    let status = if result.skipped {
        "SKIPPED"
    } else if result.success {
        "SUCCESS"
    } else {
        "FAILED"
    };

    println!("\n{}", sep);
    println!(
        "{} {} | {} | run {}",
        icon,
        result.workflow_name,
        status,
        &result.run_id[..8.min(result.run_id.len())]
    );
    println!("{}", sep);

    if result.skipped {
        println!("\nNothing to do this tick.");
    } else if result.success {
        if let Some(ref r) = result.pr_response_result {
            println!(
                "\nPR #{} — comments addressed",
                result.pr_number.unwrap_or(0)
            );
            println!("  Inline replies posted  : {}", r.inline_replies_posted);
            println!(
                "  Summary comment        : {}",
                if r.summary_posted { "posted" } else { "failed" }
            );
            println!("  New commit             : {}", r.new_head_sha);
            if !r.errors.is_empty() {
                println!("  Errors ({}):", r.errors.len());
                for e in &r.errors {
                    println!("    • {}", e);
                }
            }
        } else if let Some(ref r) = result.pr_review_result {
            println!("\nPR #{} reviewed", result.pr_number.unwrap_or(0));
            println!("  Inline comments posted : {}", r.inline_comment_count);
            println!(
                "  Summary comment        : {}",
                if r.summary_posted { "posted" } else { "failed" }
            );
            if !r.errors.is_empty() {
                println!("  Errors ({}):", r.errors.len());
                for e in &r.errors {
                    println!("    • {}", e);
                }
            }
        }
        if let Some(ref url) = result.pr_url {
            println!("\nPR: {}", url);
        }
    } else {
        println!(
            "\nError: {}",
            result.error.as_deref().unwrap_or("unknown")
        );
    }

    println!("\n{}\n", sep);
}

fn print_technique_result(technique: &str, result: &RunResult) {
    let icon = if result.success { "✓" } else { "✗" };
    let duration = result.duration_ms as f64 / 1000.0;
    println!("\n{}", "─".repeat(60));
    println!(
        "{} {} | {:.2}s | {} LLM call(s)",
        icon, technique, duration, result.llm_calls
    );
    println!("{}", "─".repeat(60));
    if result.success {
        println!("\nAnswer:\n\n{}", result.answer);
    } else {
        println!(
            "\nFailed: {}",
            result.error.as_deref().unwrap_or("unknown error")
        );
    }
    println!("\n{}\n", "─".repeat(60));
}
