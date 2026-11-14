//! `dotfilesync apply` — review a plan and apply it through the daemon.
//!
//! The preview can run in this process. The mutation always goes through dotfilesyncd, which
//! owns the journal, backups, and locks. A non-TTY session without `--yes` prints the plan
//! and exits without applying.

use anyhow::{bail, Result};
use clap::Args;
use dotfilesync_protocol::JobOutput;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::mode;
use super::plan::{self, PlanArgs};
use super::remote::{self, Remote};

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Path to the manifest (default: ./dotfilesync.toml)
    #[arg(long, default_value = "dotfilesync.toml")]
    pub manifest: PathBuf,

    /// Profile name. Omit to select from `[select]` (or the base layer).
    #[arg(long)]
    pub profile: Option<String>,

    /// Apply without prompting
    #[arg(long)]
    pub yes: bool,

    /// Allow the login-shell step to run `chsh` (never implied)
    #[arg(long)]
    pub allow_sudo: bool,

    /// Print the plan and exit. Never applies.
    #[arg(long)]
    pub dry_run: bool,

    /// Continue a run that stopped before every step finished
    #[arg(long, value_name = "RUN_ID")]
    pub resume: Option<String>,

    /// Override the home directory (primarily for testing)
    #[arg(long)]
    pub home: Option<PathBuf>,

    /// Override the repository root (defaults to the manifest's parent)
    #[arg(long)]
    pub repo_root: Option<PathBuf>,

    /// Homebrew binary for the local preview (primarily for testing). Apply itself uses the daemon's brew.
    #[arg(long, hide = true)]
    pub brew_bin: Option<PathBuf>,
}

pub fn execute(args: ApplyArgs, data_dir: Option<&Path>) -> Result<()> {
    if args.resume.is_some() {
        return execute_resume(&args, data_dir);
    }
    let preview_only = args.dry_run || (!args.yes && !io::stdin().is_terminal());
    if preview_only {
        let brew_bin = args
            .brew_bin
            .clone()
            .or_else(|| std::env::var_os("DOTFILESYNC_BREW_BIN").map(PathBuf::from));
        let planned = plan::local_plan(&plan_args(&args), brew_bin)?;
        plan::print_plan(&planned, false)?;
        if args.dry_run {
            eprintln!("Dry run: nothing was applied.");
        } else {
            eprintln!(
                "note: not a TTY and --yes was not passed; dry-run only, nothing was applied."
            );
        }
        return Ok(());
    }

    if args.brew_bin.is_some() {
        bail!("--brew-bin only applies to a local preview. Start dotfilesyncd with its own --brew-bin, then run apply without it.");
    }

    let mut client = mode::connect(data_dir)?;
    let planned = plan::daemon_plan(&mut client, &plan::plan_input(&plan_args(&args), true)?)?;
    plan::print_plan(&planned, false)?;
    if planned.is_empty() {
        eprintln!("Nothing to apply.");
        return Ok(());
    }
    if !args.yes {
        let prompt = format!("Apply {} actions? [y/N] ", planned.actions.len());
        if !confirm(&prompt)? {
            eprintln!("Not applied.");
            return Ok(());
        }
    }
    let grant = client.plan_approve(&planned.plan_id(), &planned.digest())?;
    let job = client.plan_apply(&planned.plan_id(), &grant.token, args.allow_sudo)?;
    eprintln!(
        "dotfilesyncd (pid {}) is applying plan {} as job {}.",
        client.daemon().pid,
        planned.plan_id(),
        job.job_id
    );
    match remote::follow(&mut client, &job, false)? {
        Remote::Finished(JobOutput::Apply { journal }) => {
            eprintln!("Run {} completed.", journal.run_id);
            Ok(())
        }
        Remote::Failed(job) => Err(remote::failure(&job)),
        Remote::Finished(other) => Err(remote::unexpected(&other)),
        Remote::Detached(_) => Ok(()),
    }
}

fn execute_resume(args: &ApplyArgs, data_dir: Option<&Path>) -> Result<()> {
    let run_id = args.resume.as_deref().unwrap_or_default();
    if !args.dry_run && !args.yes && !io::stdin().is_terminal() {
        eprintln!("note: not a TTY and --yes was not passed; dry-run only, nothing was resumed.");
        return Ok(());
    }
    if args.dry_run {
        let mut client = mode::connect(data_dir)?;
        let view = client.plan_run_status(run_id)?;
        println!(
            "Run {} status {} (plan {})",
            view.run_id, view.status, view.plan_id
        );
        eprintln!("Dry run: nothing was resumed.");
        return Ok(());
    }
    let mut client = mode::connect(data_dir)?;
    let view = client.plan_run_status(run_id)?;
    println!(
        "Run {} status {} (plan {})",
        view.run_id, view.status, view.plan_id
    );
    if !args.yes && io::stdin().is_terminal() {
        let prompt = format!("Resume run {}? [y/N] ", view.run_id);
        if !confirm(&prompt)? {
            eprintln!("Not resumed.");
            return Ok(());
        }
    }
    let grant = client.plan_approve(&view.plan_id, &view.plan_digest)?;
    let job = client.plan_resume(&view.run_id, &grant.token, args.allow_sudo)?;
    eprintln!(
        "dotfilesyncd (pid {}) is resuming run {} as job {}.",
        client.daemon().pid,
        view.run_id,
        job.job_id
    );
    match remote::follow(&mut client, &job, false)? {
        Remote::Finished(JobOutput::Apply { journal }) => {
            eprintln!("Run {} completed.", journal.run_id);
            Ok(())
        }
        Remote::Failed(job) => Err(remote::failure(&job)),
        Remote::Finished(other) => Err(remote::unexpected(&other)),
        Remote::Detached(_) => Ok(()),
    }
}

fn plan_args(args: &ApplyArgs) -> PlanArgs {
    PlanArgs {
        manifest: args.manifest.clone(),
        profile: args.profile.clone(),
        json: false,
        home: args.home.clone(),
        repo_root: args.repo_root.clone(),
        mode: Default::default(),
        brew_bin: None,
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}
