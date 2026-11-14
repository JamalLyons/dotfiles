//! `dotfilesync plan` — compute a deterministic plan, in-process or via the daemon.

use anyhow::Result;
use clap::Args;
use dotfilesync_core::{current_hostname, run_plan, Plan, PlanInput, RealCommandRunner};
use dotfilesync_protocol::{Client, JobOutput};
use std::path::{Path, PathBuf};

use super::mode::{self, Mode, Route};
use super::remote::{self, Remote};
use crate::output::{human, json};

#[derive(Debug, Args)]
pub struct PlanArgs {
    /// Path to the manifest (default: ./dotfilesync.toml)
    #[arg(long, default_value = "dotfilesync.toml")]
    pub manifest: PathBuf,

    /// Profile name. Omit to select from `[select]` (or the base layer).
    #[arg(long)]
    pub profile: Option<String>,

    /// Print the plan as JSON
    #[arg(long)]
    pub json: bool,

    /// Override the home directory (primarily for testing)
    #[arg(long)]
    pub home: Option<PathBuf>,

    /// Override the repository root (defaults to the manifest's parent)
    #[arg(long)]
    pub repo_root: Option<PathBuf>,

    /// Where to plan: auto (daemon if running, else in-process), daemon, or standalone
    #[arg(long, value_enum, default_value_t = Mode::Auto)]
    pub mode: Mode,

    /// Override the Homebrew binary (standalone only; primarily for testing)
    #[arg(long, hide = true)]
    pub brew_bin: Option<PathBuf>,
}

pub fn execute(args: PlanArgs, data_dir: Option<&Path>) -> Result<()> {
    let brew_bin = args
        .brew_bin
        .clone()
        .or_else(|| std::env::var_os("DOTFILESYNC_BREW_BIN").map(PathBuf::from));
    let standalone_only = brew_bin.as_ref().map(|_| "--brew-bin/DOTFILESYNC_BREW_BIN");
    let plan = match mode::route(args.mode, data_dir, "plan", standalone_only)? {
        Route::Standalone { note } => {
            if let Some(note) = note {
                eprintln!("{note}");
            }
            local_plan(&args, brew_bin)?
        }
        Route::Daemon(mut client) => daemon_plan(&mut client, &plan_input(&args, true)?)?,
    };
    print_plan(&plan, args.json)
}

pub fn print_plan(plan: &Plan, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", json::render_json(plan)?);
    } else {
        print!("{}", human::render_plan(plan));
    }
    Ok(())
}

pub fn local_plan(args: &PlanArgs, brew_bin: Option<PathBuf>) -> Result<Plan> {
    let input = plan_input(args, false)?;
    let mut runner = RealCommandRunner::default();
    if let Some(bin) = brew_bin {
        runner = runner.with_brew_binary(bin);
    }
    run_plan(&input, &runner, &current_hostname(), &mut |_| {}, &|| false)
        .map_err(anyhow::Error::from)
}

pub fn plan_input(args: &PlanArgs, absolute: bool) -> Result<PlanInput> {
    let home = args
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest = if absolute {
        mode::absolute(&args.manifest)?
    } else {
        args.manifest.clone()
    };
    let home = if absolute {
        mode::absolute(&home)?
    } else {
        home
    };
    let repo_root = match &args.repo_root {
        Some(path) if absolute => Some(mode::absolute(path)?),
        Some(path) => Some(path.clone()),
        None => None,
    };
    Ok(PlanInput {
        manifest,
        home,
        repo_root,
        profile: args.profile.clone(),
    })
}

pub fn daemon_plan(client: &mut Client, input: &PlanInput) -> Result<Plan> {
    let job = client.plan_create(input.clone())?;
    eprintln!(
        "dotfilesyncd (pid {}) is planning as job {}.",
        client.daemon().pid,
        job.job_id
    );
    let finished = client.watch_job(&job.job_id, 0, &mut |event| {
        eprintln!("  [{}] {}", event.stage, event.message);
    })?;
    match remote::finish(client, &finished)? {
        Remote::Finished(JobOutput::Plan { plan, .. }) => Ok(plan),
        Remote::Failed(job) => Err(remote::failure(&job)),
        Remote::Finished(other) => Err(remote::unexpected(&other)),
        Remote::Detached(_) => unreachable!("plan watch returns a terminal snapshot"),
    }
}
