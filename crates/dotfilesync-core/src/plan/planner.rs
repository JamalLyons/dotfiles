use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use super::model::{ActionKind, Plan, PlanAction, PlanInput, Risk, PLAN_SCHEMA};
use crate::brew::{discover_homebrew, BrewDiscoveryResult, CommandOutput, CommandRunner};
use crate::config::{ConfigFileState, ConfigInspector, ObservedConfigFile};
use crate::manifest::{
    current_os, expand_target_path, load_manifest, resolve_profile, FileMode, ProfileSelection,
    ResolvedManifest, TrackedFile,
};
use crate::ops::{OpError, OpStage};

struct Draft {
    kind: ActionKind,
    summary: String,
    risk: Risk,
    reversible: bool,
    backup: bool,
}

pub fn run_plan(
    input: &PlanInput,
    runner: &dyn CommandRunner,
    hostname: &str,
    progress: &mut dyn FnMut(OpStage),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Plan, OpError> {
    check(is_cancelled)?;
    progress(OpStage::LoadingManifest);
    let manifest = load_manifest(&input.manifest).map_err(|source| OpError::Manifest {
        path: input.manifest.clone(),
        source,
    })?;
    let selection = match &input.profile {
        Some(name) => ProfileSelection::Explicit(name.clone()),
        None => ProfileSelection::Auto,
    };
    let resolved =
        resolve_profile(&manifest, &selection, hostname, current_os()).map_err(|source| {
            OpError::Manifest {
                path: input.manifest.clone(),
                source,
            }
        })?;

    check(is_cancelled)?;
    progress(OpStage::DiscoveringHomebrew);
    let brew = discover_homebrew(runner);

    check(is_cancelled)?;
    progress(OpStage::InspectingConfigs);
    let repo_root = default_repo_root(&input.manifest, input.repo_root.as_deref());
    let inspector = ConfigInspector::new(
        &repo_root,
        &input.home,
        &resolved.exclusions,
        &resolved.overrides,
    );
    let configs = inspector.inspect_files(&resolved.files);

    check(is_cancelled)?;
    progress(OpStage::Planning);

    let login_shell = observe_login_shell(runner);

    let mut drafts = Vec::new();
    if brew.available && brew.packages_enumerated {
        push_packages(&mut drafts, &resolved, &brew);
    }
    let mut file_actions = Vec::new();
    let mut override_actions = Vec::new();
    for (file, observed) in resolved.files.iter().zip(configs.iter()) {
        if let Some(draft) = plan_file(file, observed, &resolved, &input.home) {
            if matches!(draft.kind, ActionKind::EnsureLocalOverride { .. }) {
                override_actions.push(draft);
            } else {
                file_actions.push(draft);
            }
        }
    }
    file_actions.sort_by(|a, b| action_target(&a.kind).cmp(action_target(&b.kind)));
    override_actions.sort_by(|a, b| action_target(&a.kind).cmp(action_target(&b.kind)));
    drafts.extend(file_actions);

    let mut covered = BTreeSet::new();
    for draft in &override_actions {
        covered.insert(action_target(&draft.kind).to_path_buf());
    }
    for file in &resolved.files {
        covered.insert(expand_target_path(&file.target, &input.home));
    }
    for ov in &resolved.overrides {
        let target = expand_target_path(&ov.target, &input.home);
        if covered.contains(&target) || path_present(&target) {
            continue;
        }
        override_actions.push(local_override_action(target, None));
    }
    override_actions.sort_by(|a, b| action_target(&a.kind).cmp(action_target(&b.kind)));
    drafts.extend(override_actions);

    if resolved.setup.fisher
        && !path_present(&input.home.join(".config/fish/functions/fisher.fish"))
    {
        drafts.push(Draft {
            kind: ActionKind::InstallFisher,
            summary: "install fisher".into(),
            risk: Risk::Medium,
            reversible: false,
            backup: false,
        });
        drafts.push(Draft {
            kind: ActionKind::FisherUpdate,
            summary: "fisher update".into(),
            risk: Risk::Medium,
            reversible: false,
            backup: false,
        });
    }

    if let Some(shell_name) = &resolved.setup.login_shell {
        let shell = resolve_shell(runner, brew.brew_path.as_deref(), shell_name)?;
        if login_shell != path_string(&shell) {
            drafts.push(Draft {
                kind: ActionKind::SetLoginShell {
                    shell: shell.clone(),
                },
                summary: format!("set login shell to {}", shell.display()),
                risk: Risk::High,
                reversible: true,
                backup: false,
            });
        }
    }

    let actions = drafts
        .into_iter()
        .enumerate()
        .map(|(index, draft)| PlanAction {
            id: format!("a-{:03}", index + 1),
            kind: draft.kind,
            summary: draft.summary,
            risk: draft.risk,
            reversible: draft.reversible,
            backup: draft.backup,
        })
        .collect();

    check(is_cancelled)?;
    Ok(Plan {
        schema: PLAN_SCHEMA,
        profile: resolved.profile,
        manifest_path: input.manifest.clone(),
        home: input.home.clone(),
        repo_root,
        actions,
        state_fingerprint: state_fingerprint(&brew, &configs, &login_shell),
    })
}

fn check(is_cancelled: &dyn Fn() -> bool) -> Result<(), OpError> {
    if is_cancelled() {
        Err(OpError::Cancelled)
    } else {
        Ok(())
    }
}

fn default_repo_root(manifest: &Path, repo_root: Option<&Path>) -> PathBuf {
    repo_root.map(Path::to_path_buf).unwrap_or_else(|| {
        manifest
            .parent()
            .map(Path::to_path_buf)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    })
}

fn push_packages(drafts: &mut Vec<Draft>, resolved: &ResolvedManifest, brew: &BrewDiscoveryResult) {
    let installed_formulae: HashSet<&str> = brew.formulae.iter().map(|p| p.name.as_str()).collect();
    let installed_casks: HashSet<&str> = brew.casks.iter().map(|p| p.name.as_str()).collect();

    for name in &resolved.homebrew.formulae {
        if !installed_formulae.contains(name.as_str()) {
            drafts.push(package_action(
                ActionKind::BrewInstallFormula { name: name.clone() },
                format!("brew install {name}"),
                Risk::Low,
            ));
        }
    }
    for name in &resolved.homebrew.casks {
        if !installed_casks.contains(name.as_str()) {
            drafts.push(package_action(
                ActionKind::BrewInstallCask { name: name.clone() },
                format!("brew install --cask {name}"),
                Risk::Medium,
            ));
        }
    }
}

fn package_action(kind: ActionKind, summary: String, risk: Risk) -> Draft {
    Draft {
        kind,
        summary,
        risk,
        reversible: false,
        backup: false,
    }
}

fn plan_file(
    file: &TrackedFile,
    observed: &ObservedConfigFile,
    resolved: &ResolvedManifest,
    home: &Path,
) -> Option<Draft> {
    if observed.state == ConfigFileState::Excluded {
        return None;
    }
    if is_protected(file, resolved, home) {
        if observed.state == ConfigFileState::Missing {
            let template = observed
                .source_path
                .is_file()
                .then(|| observed.source_path.clone());
            return Some(local_override_action(
                observed.target_path.clone(),
                template,
            ));
        }
        return None;
    }
    if matches!(
        observed.state,
        ConfigFileState::SourceMissing
            | ConfigFileState::SourceOutsideRoot
            | ConfigFileState::Unreadable
            | ConfigFileState::MachineLocal
    ) {
        return None;
    }

    let converged = match file.mode {
        FileMode::Copy => observed.state == ConfigFileState::Matching,
        FileMode::Symlink => observed.state == ConfigFileState::SymlinkToSource,
    };
    if converged {
        return None;
    }
    let exists = observed.state != ConfigFileState::Missing;
    let summary = format!(
        "{} {} -> {}",
        match file.mode {
            FileMode::Copy => "copy",
            FileMode::Symlink => "link",
        },
        observed.source_path.display(),
        observed.target_path.display()
    );
    let kind = match file.mode {
        FileMode::Copy => ActionKind::CopyFile {
            source: observed.source_path.clone(),
            target: observed.target_path.clone(),
        },
        FileMode::Symlink => ActionKind::LinkFile {
            source: observed.source_path.clone(),
            target: observed.target_path.clone(),
        },
    };
    Some(Draft {
        kind,
        summary,
        risk: if exists { Risk::Medium } else { Risk::Low },
        reversible: true,
        backup: exists,
    })
}

fn local_override_action(target: PathBuf, template: Option<PathBuf>) -> Draft {
    let summary = format!("ensure local override {}", target.display());
    Draft {
        kind: ActionKind::EnsureLocalOverride { target, template },
        summary,
        risk: Risk::Low,
        reversible: true,
        backup: false,
    }
}

fn is_protected(file: &TrackedFile, resolved: &ResolvedManifest, home: &Path) -> bool {
    if file.machine_local || is_local_fish(&file.source) || is_local_fish(&file.target) {
        return true;
    }
    let expanded = expand_target_path(&file.target, home);
    resolved
        .overrides
        .iter()
        .any(|ov| ov.target == file.target || expand_target_path(&ov.target, home) == expanded)
}

fn is_local_fish(path: &str) -> bool {
    Path::new(path).file_name().and_then(|name| name.to_str()) == Some("local.fish")
}

fn action_target(kind: &ActionKind) -> &Path {
    match kind {
        ActionKind::CopyFile { target, .. }
        | ActionKind::LinkFile { target, .. }
        | ActionKind::EnsureLocalOverride { target, .. } => target,
        _ => Path::new(""),
    }
}

fn path_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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

fn resolve_shell(
    runner: &dyn CommandRunner,
    brew_path: Option<&Path>,
    name: &str,
) -> Result<PathBuf, OpError> {
    let brew_path = brew_path.ok_or_else(|| {
        OpError::Failed(
            "Homebrew executable not found; cannot resolve login shell via brew prefix".into(),
        )
    })?;
    let output = run_brew(runner, brew_path, &["--prefix"])
        .map_err(|error| OpError::Failed(format!("failed to resolve Homebrew prefix: {error}")))?;
    let prefix = output.stdout.lines().next().unwrap_or("").trim();
    if prefix.is_empty() {
        return Err(OpError::Failed(
            "brew --prefix returned an empty path".into(),
        ));
    }
    Ok(PathBuf::from(prefix).join("bin").join(name))
}

fn run_brew(
    runner: &dyn CommandRunner,
    brew_path: &Path,
    args: &[&str],
) -> Result<CommandOutput, String> {
    let program = brew_path
        .to_str()
        .ok_or_else(|| format!("brew path '{}' is not valid UTF-8", brew_path.display()))?;
    runner.run(program, args).map_err(|error| error.to_string())
}

fn state_fingerprint(
    brew: &BrewDiscoveryResult,
    configs: &[ObservedConfigFile],
    login_shell: &str,
) -> String {
    let mut lines = Vec::new();
    let mut formulae = brew.formulae.clone();
    formulae.sort_by(|a, b| a.name.cmp(&b.name));
    for pkg in formulae {
        lines.push(format!(
            "formula\t{}\t{}",
            pkg.name,
            pkg.version.unwrap_or_default()
        ));
    }
    let mut casks = brew.casks.clone();
    casks.sort_by(|a, b| a.name.cmp(&b.name));
    for pkg in casks {
        lines.push(format!(
            "cask\t{}\t{}",
            pkg.name,
            pkg.version.unwrap_or_default()
        ));
    }
    let mut configs = configs.to_vec();
    configs.sort_by(|a, b| a.target_path.cmp(&b.target_path));
    for cfg in configs {
        lines.push(format!(
            "file\t{}\t{}",
            cfg.target_path.display(),
            observed_token(&cfg)
        ));
    }
    lines.push(format!("shell\t{login_shell}"));
    hex::encode(Sha256::digest(lines.join("\n").as_bytes()))
}

fn observed_token(cfg: &ObservedConfigFile) -> String {
    match cfg.state {
        ConfigFileState::Missing | ConfigFileState::SourceMissing => "missing".into(),
        ConfigFileState::SymlinkToSource
        | ConfigFileState::SymlinkExternal
        | ConfigFileState::BrokenSymlink => format!(
            "symlink:{}",
            cfg.symlink_target
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        ),
        _ => format!("sha:{}", cfg.target_hash.as_deref().unwrap_or("")),
    }
}
