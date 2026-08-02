use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use codex_resume_tracker::app_server::CodexAppServer;
use codex_resume_tracker::config::{Config, OnClose, SourceKind, SummaryInput, SummaryProvider};
use codex_resume_tracker::db::Database;
use codex_resume_tracker::doctor;
use codex_resume_tracker::hook;
use codex_resume_tracker::paths::AppPaths;
use codex_resume_tracker::setup::{self, HookChoice};
use codex_resume_tracker::sync::{self, SyncReport};
use codex_resume_tracker::tui;

#[derive(Debug, Parser)]
#[command(name = "codex-resume-tracker", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Configure providers, source filters, and the SessionEnd hook.
    Setup(SetupArgs),
    /// Import and locally enrich matching non-archived Codex threads.
    Sync,
    /// Regenerate a thread title and summary.
    Summarize(SummarizeArgs),
    /// Validate Codex, storage, terminal, hook, and provider configuration.
    Doctor,
    /// Receive SessionEnd JSON on stdin.
    #[command(hide = true)]
    Hook,
    /// Process one queued enrichment job.
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        thread_id: String,
    },
}

#[derive(Clone, Debug, Default, Args)]
struct SetupArgs {
    /// Accept all documented defaults instead of prompting.
    #[arg(long)]
    defaults: bool,
    /// Override the summary provider: local, codex, or openai.
    #[arg(long)]
    provider: Option<SummaryProvider>,
    /// Override summary input: 16k, 64k, entire, or a positive number.
    #[arg(long = "input")]
    summary_input: Option<SummaryInput>,
    /// Override on-close behavior: automatic, local-first, or manual.
    #[arg(long)]
    on_close: Option<OnClose>,
    /// Override visible sources; repeat or comma-separate cli,vscode,exec.
    #[arg(long = "source", value_delimiter = ',')]
    sources: Vec<SourceKind>,
    /// Hook behavior: automatic, manual, or skip.
    #[arg(long)]
    hook: Option<HookChoice>,
    /// Save setup without importing existing threads.
    #[arg(long)]
    no_import: bool,
    /// Summarize every imported thread after the local-only import.
    #[arg(long)]
    summarize_all: bool,
    /// Confirm summarize-all usage without an interactive prompt.
    #[arg(long, requires = "summarize_all")]
    yes: bool,
}

#[derive(Clone, Debug, Args)]
struct SummarizeArgs {
    /// Thread ID to summarize.
    #[arg(
        value_name = "ID",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    id: Option<String>,
    /// Summarize every matching non-archived thread.
    #[arg(long)]
    all: bool,
    /// Override the configured provider for this run.
    #[arg(long)]
    provider: Option<SummaryProvider>,
    /// Override the configured character cap for this run.
    #[arg(long = "input")]
    summary_input: Option<SummaryInput>,
    /// Confirm summarize-all usage without an interactive prompt.
    #[arg(long)]
    yes: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    let executable = std::env::current_exe().context("resolve tracker executable")?;
    match cli.command {
        None => {
            if !paths.config().exists() {
                if !io::stdin().is_terminal() {
                    bail!(
                        "tracker is not configured; run `codex-resume-tracker setup` in a terminal"
                    );
                }
                run_setup(&paths, &executable, SetupArgs::default())?;
            }
            tui::run(&paths, Config::load(&paths)?)
        }
        Some(Command::Setup(args)) => run_setup(&paths, &executable, args),
        Some(Command::Sync) => run_sync(&paths),
        Some(Command::Summarize(args)) => run_summarize(&paths, args),
        Some(Command::Doctor) => run_doctor(&paths, &executable),
        Some(Command::Hook) => {
            hook::run_hook(io::stdin().lock(), &paths, &executable)?;
            Ok(())
        }
        Some(Command::Worker { thread_id }) => hook::run_worker(&paths, &thread_id),
    }
}

fn run_setup(paths: &AppPaths, executable: &std::path::Path, args: SetupArgs) -> Result<()> {
    let interactive = !args.defaults;
    let (mut config, mut hook_choice) = if interactive {
        let mut input = io::stdin().lock();
        let mut output = io::stdout().lock();
        setup::interactive_wizard(&mut input, &mut output)?
    } else {
        (Config::default(), HookChoice::Automatic)
    };

    if let Some(provider) = args.provider {
        config.summary_provider = provider;
    }
    if let Some(input) = args.summary_input {
        config.summary_input = input;
    }
    if let Some(on_close) = args.on_close {
        config.on_close = on_close;
    }
    if !args.sources.is_empty() {
        config.sources = deduplicate_sources(args.sources);
    }
    if let Some(choice) = args.hook {
        hook_choice = choice;
    }
    config.save(paths)?;
    println!("\nSaved {}", paths.config().display());
    println!(
        "Provider={} input={} on-close={} sources={}",
        config.summary_provider,
        config.summary_input,
        config.on_close,
        config
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );

    println!("\nSessionEnd hook JSON (always shown for review):");
    println!("{}", setup::render_hook_snippet(executable)?);
    match hook_choice {
        HookChoice::Automatic => {
            let path = setup::hooks_path()?;
            let outcome = setup::merge_hook_file(&path, executable)?;
            println!(
                "{} {}",
                if outcome.changed {
                    "Merged hook into"
                } else {
                    "Hook already present in"
                },
                outcome.path.display()
            );
        }
        HookChoice::Manual => println!("No hook file changed; merge the JSON above manually."),
        HookChoice::Skip => println!("Hook installation skipped. See {}", setup::HOOKS_DOC_URL),
    }
    println!("Open Codex and run /hooks to review and trust the exact command before it can run.");

    if args.no_import {
        println!("Initial import skipped; run `codex-resume-tracker sync` later.");
        return Ok(());
    }

    let database = Database::open(paths.database())?;
    let mut server = CodexAppServer::connect()?;
    let report = sync::sync_all(&database, &mut server, &config, paths)?;
    print_report("Initial local-only import", &report);
    println!("No Codex or OpenAI summary provider was used during initial import.");
    println!(
        "On-demand: `codex-resume-tracker summarize <id>` or `codex-resume-tracker summarize --all`."
    );

    let should_summarize = if args.summarize_all {
        args.yes || confirm_all_usage(config.summary_provider)?
    } else if interactive && config.summary_provider.is_external() && report.total() > 0 {
        confirm_all_usage(config.summary_provider)?
    } else {
        false
    };
    if should_summarize {
        let report = sync::summarize_all(
            &database,
            &mut server,
            &config,
            paths,
            config.summary_provider,
            config.summary_input,
        )?;
        print_report("Summarize all", &report);
    }
    Ok(())
}

fn run_sync(paths: &AppPaths) -> Result<()> {
    let config = Config::load(paths)?;
    let database = Database::open(paths.database())?;
    let mut server = CodexAppServer::connect()?;
    let report = sync::sync_all(&database, &mut server, &config, paths)?;
    print_report("Sync (local extraction only)", &report);
    Ok(())
}

fn run_summarize(paths: &AppPaths, args: SummarizeArgs) -> Result<()> {
    let config = Config::load(paths)?;
    let provider = args.provider.unwrap_or(config.summary_provider);
    let cap = args.summary_input.unwrap_or(config.summary_input);
    let database = Database::open(paths.database())?;

    if args.all && !args.yes && !confirm_all_usage(provider)? {
        println!("Cancelled; no summaries were generated.");
        return Ok(());
    }

    let mut server = CodexAppServer::connect()?;
    if args.all {
        let report = sync::summarize_all(&database, &mut server, &config, paths, provider, cap)?;
        print_report("Summarize all", &report);
    } else if let Some(thread_id) = args.id {
        sync::summarize_one(
            &database,
            &mut server,
            &config,
            paths,
            &thread_id,
            provider,
            cap,
        )?;
        println!("Summarized {thread_id} with provider={provider} input={cap}");
    }
    Ok(())
}

fn run_doctor(paths: &AppPaths, executable: &std::path::Path) -> Result<()> {
    let checks = doctor::run(paths, executable);
    for check in &checks {
        println!("[{:<4}] {:<16} {}", check.level, check.name, check.detail);
    }
    if doctor::has_failures(&checks) {
        bail!("doctor found one or more required checks that failed");
    }
    Ok(())
}

fn confirm_all_usage(provider: SummaryProvider) -> Result<bool> {
    let usage = match provider {
        SummaryProvider::Local => {
            "The local provider makes no Codex/API call, but will replace every generated summary."
        }
        SummaryProvider::Codex => {
            "Each thread starts an ephemeral Codex execution and consumes Codex model usage."
        }
        SummaryProvider::Openai => {
            "Each thread calls the OpenAI Responses API and may incur API charges."
        }
    };
    eprintln!("\nWARNING: summarize --all selected provider={provider}.");
    eprintln!("{usage}");
    eprint!("Continue for all matching threads? [y/N]: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_report(label: &str, report: &SyncReport) {
    println!(
        "{label}: {} imported, {} updated, {} summarized, {} error(s)",
        report.imported,
        report.updated,
        report.summarized,
        report.errors.len()
    );
    for error in &report.errors {
        eprintln!("  - {error}");
    }
}

fn deduplicate_sources(sources: Vec<SourceKind>) -> Vec<SourceKind> {
    let mut output = Vec::new();
    for source in sources {
        if !output.contains(&source) {
            output.push(source);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_requires_exactly_one_target_but_allows_overrides() {
        assert!(Cli::try_parse_from(["tracker", "summarize", "--provider", "local"]).is_err());
        assert!(Cli::try_parse_from([
            "tracker",
            "summarize",
            "--all",
            "--provider",
            "local",
            "--input",
            "16k",
            "--yes"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["tracker", "summarize", "thr_123", "--all"]).is_err());
    }
}
