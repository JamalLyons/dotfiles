//! Apply a [`Plan`] one step at a time, journaling each transition.

mod backup;
mod journal;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::brew::CommandRunner;
use crate::manifest::{
    current_hostname, current_os, expand_target_path, load_manifest, resolve_profile,
    ProfileSelection,
};
use crate::plan::{ActionKind, Plan};

use backup::{backup_existing, ensure_dir, write_atomic};
use journal::JournalStore;

/// Fixed Fisher install script from `lib/fish.sh` (`fish -c` argument, not caller input).
const FISHER_INSTALL_SCRIPT: &str = "curl -sL https://raw.githubusercontent.com/jorgebucaran/fisher/main/functions/fisher.fish | source && fisher install jorgebucaran/fisher";

/// Fixed `fisher update` script from `lib/fish.sh`.
const FISHER_UPDATE_SCRIPT: &str = "type -q fisher; and fisher update";

const LOCAL_OVERRIDE_STUB: &str = "\
# Local / private Fish environment variables (not managed by setup re-runs)
# Add: set -gx MY_VAR \"value\"
";

const SUDO_REQUIRED: &str = "requires sudo; rerun with --allow-sudo";

/// Runtime handles for one apply or resume.
pub struct ApplyContext<'a> {
    pub runner: &'a dyn CommandRunner,
    pub backup_root: PathBuf,
    /// Directory that holds `journal.jsonl` and `plan.json`.
    pub run_dir: PathBuf,
    pub allow_sudo: bool,
    pub now: &'a dyn Fn() -> u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StepRecord {
    pub action_id: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at_ms: u64,
    /// Previous login shell captured before `chsh`, when this step changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_shell: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunJournal {
    pub run_id: String,
    pub plan_digest: String,
    pub steps: Vec<StepRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("operation was cancelled")]
    Cancelled { journal: RunJournal },

    #[error("plan digest mismatch: journal has {found}, plan is {expected}")]
    DigestMismatch { found: String, expected: String },

    #[error("{0}")]
    Journal(String),

    #[error("step {action_id} failed: {message}")]
    StepFailed {
        action_id: String,
        message: String,
        journal: Box<RunJournal>,
    },
}

struct Outcome {
    status: StepStatus,
    backup_path: Option<PathBuf>,
    previous_shell: Option<String>,
}

pub fn apply_plan(
    plan: &Plan,
    ctx: &ApplyContext<'_>,
    progress: &mut dyn FnMut(&StepRecord),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<RunJournal, ApplyError> {
    let digest = plan.digest();
    let run_id = make_run_id(&digest, (ctx.now)());
    ensure_dir(&ctx.run_dir).map_err(|error| ApplyError::Journal(error.to_string()))?;
    ensure_dir(&ctx.backup_root).map_err(|error| ApplyError::Journal(error.to_string()))?;
    let store = JournalStore::create(&ctx.run_dir, &run_id, &digest, plan)?;
    let pending = plan
        .actions
        .iter()
        .map(|action| StepRecord {
            action_id: action.id.clone(),
            status: StepStatus::Pending,
            backup_path: None,
            error: None,
            at_ms: 0,
            previous_shell: None,
        })
        .collect();
    drive(
        plan,
        ctx,
        &store,
        &run_id,
        &digest,
        pending,
        progress,
        is_cancelled,
    )
}

pub fn resume_run(
    run_dir: &Path,
    plan: &Plan,
    ctx: &ApplyContext<'_>,
    progress: &mut dyn FnMut(&StepRecord),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<RunJournal, ApplyError> {
    let store = JournalStore::open(run_dir)?;
    let (run_id, digest, previous) = store.load()?;
    let expected = plan.digest();
    if digest != expected {
        return Err(ApplyError::DigestMismatch {
            found: digest,
            expected,
        });
    }
    let steps = plan
        .actions
        .iter()
        .map(|action| {
            previous
                .iter()
                .rev()
                .find(|step| step.action_id == action.id)
                .cloned()
                .unwrap_or(StepRecord {
                    action_id: action.id.clone(),
                    status: StepStatus::Pending,
                    backup_path: None,
                    error: None,
                    at_ms: 0,
                    previous_shell: None,
                })
        })
        .collect();
    drive(
        plan,
        ctx,
        &store,
        &run_id,
        &digest,
        steps,
        progress,
        is_cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn drive(
    plan: &Plan,
    ctx: &ApplyContext<'_>,
    store: &JournalStore,
    run_id: &str,
    digest: &str,
    mut steps: Vec<StepRecord>,
    progress: &mut dyn FnMut(&StepRecord),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<RunJournal, ApplyError> {
    for index in 0..plan.actions.len() {
        if is_cancelled() {
            return Err(ApplyError::Cancelled {
                journal: snapshot(run_id, digest, &steps),
            });
        }
        if matches!(steps[index].status, StepStatus::Done | StepStatus::Skipped) {
            continue;
        }
        let running = StepRecord {
            action_id: plan.actions[index].id.clone(),
            status: StepStatus::Running,
            backup_path: None,
            error: None,
            at_ms: (ctx.now)(),
            previous_shell: None,
        };
        store.append_step(&running)?;
        steps[index] = running.clone();
        progress(&running);

        let record = match execute(&plan.actions[index].kind, plan, ctx, run_id) {
            Ok(outcome) => StepRecord {
                action_id: running.action_id.clone(),
                status: outcome.status,
                backup_path: outcome.backup_path,
                error: None,
                at_ms: (ctx.now)(),
                previous_shell: outcome.previous_shell,
            },
            Err(message) => StepRecord {
                action_id: running.action_id.clone(),
                status: StepStatus::Failed,
                backup_path: None,
                error: Some(message),
                at_ms: (ctx.now)(),
                previous_shell: None,
            },
        };
        store.append_step(&record)?;
        let failed = record.status == StepStatus::Failed;
        let action_id = record.action_id.clone();
        let message = record.error.clone().unwrap_or_default();
        steps[index] = record.clone();
        progress(&record);
        if failed {
            return Err(ApplyError::StepFailed {
                action_id,
                message,
                journal: Box::new(snapshot(run_id, digest, &steps)),
            });
        }
    }
    Ok(snapshot(run_id, digest, &steps))
}

fn snapshot(run_id: &str, digest: &str, steps: &[StepRecord]) -> RunJournal {
    RunJournal {
        run_id: run_id.to_string(),
        plan_digest: digest.to_string(),
        steps: steps.to_vec(),
    }
}

fn execute(
    kind: &ActionKind,
    plan: &Plan,
    ctx: &ApplyContext<'_>,
    run_id: &str,
) -> Result<Outcome, String> {
    match kind {
        ActionKind::BrewInstallFormula { name } => {
            brew(ctx, &["install", name])?;
            Ok(done())
        }
        ActionKind::BrewInstallCask { name } => {
            brew(ctx, &["install", "--cask", name])?;
            Ok(done())
        }
        ActionKind::CopyFile { source, target } => {
            refuse_managed_target(plan, target)?;
            let backup_path = backup_if_present(plan, ctx, run_id, target)?;
            let bytes = fs::read(source).map_err(|error| {
                format!("failed to read source '{}': {error}", source.display())
            })?;
            let mode = file_mode(source).unwrap_or(0o644);
            write_atomic(target, &bytes, mode)?;
            Ok(Outcome {
                status: StepStatus::Done,
                backup_path,
                previous_shell: None,
            })
        }
        ActionKind::LinkFile { source, target } => {
            refuse_managed_target(plan, target)?;
            let backup_path = backup_if_present(plan, ctx, run_id, target)?;
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            std::os::unix::fs::symlink(source, target)
                .map_err(|error| format!("failed to symlink '{}': {error}", target.display()))?;
            Ok(Outcome {
                status: StepStatus::Done,
                backup_path,
                previous_shell: None,
            })
        }
        ActionKind::EnsureLocalOverride { target, template } => {
            if path_present(target) {
                return Ok(Outcome {
                    status: StepStatus::Skipped,
                    backup_path: None,
                    previous_shell: None,
                });
            }
            let bytes = match template {
                Some(path) if path.is_file() => fs::read(path).map_err(|error| {
                    format!(
                        "failed to read override template '{}': {error}",
                        path.display()
                    )
                })?,
                _ => LOCAL_OVERRIDE_STUB.as_bytes().to_vec(),
            };
            write_atomic(target, &bytes, 0o644)?;
            Ok(done())
        }
        ActionKind::InstallFisher => {
            run_program(ctx.runner, "fish", &["-c", FISHER_INSTALL_SCRIPT])?;
            Ok(done())
        }
        ActionKind::FisherUpdate => {
            run_program(ctx.runner, "fish", &["-c", FISHER_UPDATE_SCRIPT])?;
            Ok(done())
        }
        ActionKind::SetLoginShell { shell } => set_login_shell(ctx, shell),
    }
}

fn done() -> Outcome {
    Outcome {
        status: StepStatus::Done,
        backup_path: None,
        previous_shell: None,
    }
}

fn brew(ctx: &ApplyContext<'_>, args: &[&str]) -> Result<(), String> {
    let brew_path = ctx
        .runner
        .find_executable("brew")
        .ok_or_else(|| "Homebrew executable ('brew') not found".to_string())?;
    let program = brew_path
        .to_str()
        .ok_or_else(|| format!("brew path '{}' is not valid UTF-8", brew_path.display()))?;
    run_program(ctx.runner, program, args)
}

fn run_program(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<(), String> {
    runner
        .run(program, args)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn backup_if_present(
    plan: &Plan,
    ctx: &ApplyContext<'_>,
    run_id: &str,
    target: &Path,
) -> Result<Option<PathBuf>, String> {
    if path_present(target) {
        backup_existing(&plan.home, &ctx.backup_root, run_id, target, (ctx.now)()).map(Some)
    } else {
        Ok(None)
    }
}

fn path_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn refuse_managed_target(plan: &Plan, target: &Path) -> Result<(), String> {
    if target.file_name().and_then(|name| name.to_str()) == Some("local.fish") {
        return Err(format!(
            "refusing to replace local override '{}'",
            target.display()
        ));
    }
    let manifest = load_manifest(&plan.manifest_path)
        .map_err(|error| format!("failed to reload manifest: {error}"))?;
    let selection = match &plan.profile {
        Some(name) => ProfileSelection::Explicit(name.clone()),
        None => ProfileSelection::BaseOnly,
    };
    let resolved = resolve_profile(&manifest, &selection, &current_hostname(), current_os())
        .map_err(|error| format!("failed to resolve manifest: {error}"))?;
    for ov in &resolved.overrides {
        let expanded = expand_target_path(&ov.target, &plan.home);
        if expanded == target {
            return Err(format!(
                "refusing to replace machine override '{}'",
                target.display()
            ));
        }
    }
    for excluded in &resolved.exclusions.paths {
        let expanded = expand_target_path(excluded, &plan.home);
        if expanded == target || Path::new(excluded) == target {
            return Err(format!(
                "refusing to replace excluded path '{}'",
                target.display()
            ));
        }
    }
    let target_text = target.to_string_lossy();
    for pattern in &resolved.exclusions.patterns {
        let Ok(glob) = glob::Pattern::new(pattern) else {
            continue;
        };
        let file_name = target.file_name().and_then(|name| name.to_str());
        if glob.matches(target_text.as_ref()) || file_name.is_some_and(|name| glob.matches(name)) {
            return Err(format!(
                "refusing to replace excluded path '{}'",
                target.display()
            ));
        }
    }
    Ok(())
}

fn set_login_shell(ctx: &ApplyContext<'_>, shell: &Path) -> Result<Outcome, String> {
    let shell_text = shell
        .to_str()
        .ok_or_else(|| format!("login shell path '{}' is not valid UTF-8", shell.display()))?
        .to_string();
    if !shell_listed(&shell_text)? && !ctx.allow_sudo {
        return Err(SUDO_REQUIRED.to_string());
    }
    if !shell_listed(&shell_text)? {
        append_shell(ctx, &shell_text)?;
    }
    let previous = observe_login_shell(ctx.runner);
    run_program(ctx.runner, "chsh", &["-s", &shell_text])?;
    Ok(Outcome {
        status: StepStatus::Done,
        backup_path: None,
        previous_shell: if previous.is_empty() {
            None
        } else {
            Some(previous)
        },
    })
}

fn shell_listed(shell: &str) -> Result<bool, String> {
    let text = fs::read_to_string("/etc/shells")
        .map_err(|error| format!("failed to read /etc/shells: {error}"))?;
    Ok(text.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed == shell
    }))
}

fn append_shell(ctx: &ApplyContext<'_>, shell: &str) -> Result<(), String> {
    // Constant script. The shell path is a separate argument, never interpolated into it.
    const SCRIPT: &str = r#"printf '%s\n' "$1" >> /etc/shells"#;
    run_program(
        ctx.runner,
        "sudo",
        &["--", "/bin/sh", "-c", SCRIPT, "sh", shell],
    )
}

fn observe_login_shell(runner: &dyn CommandRunner) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let record = format!("/Users/{user}");
    match runner.run("dscl", &[".", "-read", &record, "UserShell"]) {
        Ok(output) => output
            .stdout
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    }
}

fn make_run_id(digest: &str, now: u64) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(digest.as_bytes());
    hasher.update(now.to_be_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("r-{}", &hex[..16])
}

/// Write `bytes` to `path`, creating parent directories mode `0755`.
pub(crate) fn sync_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("failed to open '{}': {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("failed to write '{}': {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("failed to fsync '{}': {error}", path.display()))?;
    Ok(())
}
