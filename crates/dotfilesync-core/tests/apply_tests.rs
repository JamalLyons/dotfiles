use dotfilesync_core::{
    apply_plan, resume_run, run_plan, ActionKind, ApplyContext, ApplyError, CommandOutput,
    MockCommandRunner, Plan, PlanAction, PlanInput, Risk, RunnerError, StepStatus,
};
use std::fs;
use std::path::{Path, PathBuf};

fn output(stdout: &str) -> Result<CommandOutput, RunnerError> {
    Ok(CommandOutput {
        status_code: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    })
}

fn brew_base(json: &str) -> MockCommandRunner {
    let brew = "/mock/bin/brew";
    MockCommandRunner::new()
        .with_executable("brew", Some(PathBuf::from(brew)))
        .with_response(brew, &["--version"], output("Homebrew 4.2.0\n"))
        .with_response(brew, &["info", "--json=v2", "--installed"], output(json))
}

fn install_ok(runner: MockCommandRunner, name: &str) -> MockCommandRunner {
    runner.with_response("/mock/bin/brew", &["install", name], output(""))
}

fn context<'a>(
    runner: &'a dyn dotfilesync_core::CommandRunner,
    backup_root: PathBuf,
    run_dir: PathBuf,
    allow_sudo: bool,
    now: &'a dyn Fn() -> u64,
) -> ApplyContext<'a> {
    ApplyContext {
        runner,
        backup_root,
        run_dir,
        allow_sudo,
        now,
    }
}

fn manifest_with(repo: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(repo).unwrap();
    let path = repo.join("dotfilesync.toml");
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn backup_is_created_for_a_differing_file_and_journal_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::write(repo.join("config/app.conf"), "desired\n").unwrap();
    fs::write(home.join(".config/app.conf"), "original\n").unwrap();
    let manifest = manifest_with(
        &repo,
        r#"
schema_version = 1
[[files]]
source = "config/app.conf"
target = "~/.config/app.conf"
"#,
    );
    let runner = MockCommandRunner::new();
    let input = PlanInput {
        manifest,
        home: home.clone(),
        repo_root: Some(repo.clone()),
        profile: None,
    };
    let plan = run_plan(&input, &runner, "host", &mut |_| {}, &|| false).unwrap();
    assert_eq!(plan.actions.len(), 1);
    assert!(plan.actions[0].backup);
    assert!(matches!(plan.actions[0].kind, ActionKind::CopyFile { .. }));

    let backup_root = tmp.path().join("backups");
    let run_dir = tmp.path().join("run");
    let now = || 1_700_000_000_000;
    let ctx = context(&runner, backup_root.clone(), run_dir.clone(), false, &now);
    let journal = apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap();
    assert_eq!(journal.steps.len(), 1);
    assert_eq!(journal.steps[0].status, StepStatus::Done);
    assert_eq!(
        fs::read_to_string(home.join(".config/app.conf")).unwrap(),
        "desired\n"
    );
    let backup = journal.steps[0].backup_path.clone().expect("backup path");
    assert!(backup.starts_with(&backup_root), "{}", backup.display());
    assert!(
        backup.to_string_lossy().contains(".backup."),
        "{}",
        backup.display()
    );
    assert_eq!(fs::read_to_string(&backup).unwrap(), "original\n");
    let journal_text = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert!(journal_text.contains(&plan.actions[0].id));
    assert!(journal_text.contains(&plan.digest()));
    assert!(run_dir.join("plan.json").is_file());
}

#[test]
fn resume_skips_completed_steps_after_step_three_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let manifest = manifest_with(
        &repo,
        r#"
schema_version = 1
[package_managers.homebrew]
formulae = ["alpha", "beta", "gamma"]
"#,
    );
    let json = r#"{"formulae":[],"casks":[]}"#;
    let failing = install_ok(install_ok(brew_base(json), "alpha"), "beta").with_response(
        "/mock/bin/brew",
        &["install", "gamma"],
        Err(RunnerError::NonZeroExit {
            program: "brew".into(),
            code: 1,
            stderr: "gamma failed".into(),
        }),
    );
    let input = PlanInput {
        manifest,
        home: tmp.path().join("home"),
        repo_root: Some(repo),
        profile: None,
    };
    fs::create_dir_all(&input.home).unwrap();
    let plan = run_plan(&input, &failing, "host", &mut |_| {}, &|| false).unwrap();
    assert_eq!(plan.actions.len(), 3);

    let run_dir = tmp.path().join("run");
    let now = || 10_u64;
    let ctx = context(
        &failing,
        tmp.path().join("backups"),
        run_dir.clone(),
        false,
        &now,
    );
    let err = apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap_err();
    let ApplyError::StepFailed {
        action_id, journal, ..
    } = err
    else {
        panic!("expected step failure");
    };
    assert_eq!(action_id, "a-003");
    assert_eq!(journal.steps[0].status, StepStatus::Done);
    assert_eq!(journal.steps[1].status, StepStatus::Done);
    assert_eq!(journal.steps[2].status, StepStatus::Failed);

    let retry = install_ok(
        MockCommandRunner::new().with_executable("brew", Some(PathBuf::from("/mock/bin/brew"))),
        "gamma",
    );
    let ctx = context(
        &retry,
        tmp.path().join("backups"),
        run_dir.clone(),
        false,
        &now,
    );
    let journal = resume_run(&run_dir, &plan, &ctx, &mut |_| {}, &|| false).unwrap();
    assert!(journal
        .steps
        .iter()
        .all(|step| step.status == StepStatus::Done));
}

#[test]
fn local_fish_is_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(repo.join("fish/conf.d")).unwrap();
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::create_dir_all(home.join(".config/fish/conf.d")).unwrap();
    fs::write(repo.join("fish/conf.d/local.fish"), "from-repo\n").unwrap();
    fs::write(repo.join("config/app.conf"), "desired\n").unwrap();
    fs::write(home.join(".config/fish/conf.d/local.fish"), "SECRET\n").unwrap();
    let manifest = manifest_with(
        &repo,
        r#"
schema_version = 1

[[files]]
source = "config/app.conf"
target = "~/.config/app.conf"

[[files]]
source = "fish/conf.d/local.fish"
target = "~/.config/fish/conf.d/local.fish"

[[overrides]]
target = "~/.config/fish/conf.d/local.fish"
"#,
    );
    fs::create_dir_all(home.join(".config")).unwrap();
    let runner = MockCommandRunner::new();
    let input = PlanInput {
        manifest,
        home: home.clone(),
        repo_root: Some(repo),
        profile: None,
    };
    let plan = run_plan(&input, &runner, "host", &mut |_| {}, &|| false).unwrap();
    assert!(!plan.actions.iter().any(|action| matches!(
        &action.kind,
        ActionKind::CopyFile { target, .. } | ActionKind::LinkFile { target, .. }
            if target.ends_with("local.fish")
    )));
    let now = || 20_u64;
    let ctx = context(
        &runner,
        tmp.path().join("backups"),
        tmp.path().join("run"),
        false,
        &now,
    );
    apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap();
    assert_eq!(
        fs::read_to_string(home.join(".config/fish/conf.d/local.fish")).unwrap(),
        "SECRET\n"
    );
}

#[test]
fn replan_after_apply_is_empty_on_a_converged_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::write(repo.join("config/app.conf"), "same\n").unwrap();
    let manifest = manifest_with(
        &repo,
        r#"
schema_version = 1
[package_managers.homebrew]
formulae = ["git"]
[[files]]
source = "config/app.conf"
target = "~/.config/app.conf"
"#,
    );
    let runner =
        brew_base(r#"{"formulae":[{"name":"git","installed":[{"version":"2.45.0"}]}],"casks":[]}"#);
    let input = PlanInput {
        manifest,
        home: home.clone(),
        repo_root: Some(repo),
        profile: None,
    };
    let plan = run_plan(&input, &runner, "host", &mut |_| {}, &|| false).unwrap();
    assert_eq!(plan.actions.len(), 1);
    let now = || 30_u64;
    let ctx = context(
        &runner,
        tmp.path().join("backups"),
        tmp.path().join("run"),
        false,
        &now,
    );
    apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap();
    let again = run_plan(&input, &runner, "host", &mut |_| {}, &|| false).unwrap();
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn symlink_mode_points_target_at_source() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::write(repo.join("config/app.conf"), "linked\n").unwrap();
    let manifest = manifest_with(
        &repo,
        r#"
schema_version = 1
[[files]]
source = "config/app.conf"
target = "~/.config/app.conf"
mode = "symlink"
"#,
    );
    let runner = MockCommandRunner::new();
    let input = PlanInput {
        manifest,
        home: home.clone(),
        repo_root: Some(repo.clone()),
        profile: None,
    };
    let plan = run_plan(&input, &runner, "host", &mut |_| {}, &|| false).unwrap();
    assert!(matches!(plan.actions[0].kind, ActionKind::LinkFile { .. }));
    assert!(!plan.actions[0].backup);
    let now = || 40_u64;
    let ctx = context(
        &runner,
        tmp.path().join("backups"),
        tmp.path().join("run"),
        false,
        &now,
    );
    apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap();
    let target = home.join(".config/app.conf");
    assert!(fs::symlink_metadata(&target)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(&target).unwrap(),
        repo.join("config/app.conf")
    );
}

#[test]
fn login_shell_without_sudo_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = Plan {
        schema: 1,
        profile: None,
        manifest_path: tmp.path().join("missing.toml"),
        home: tmp.path().join("home"),
        repo_root: tmp.path().join("repo"),
        actions: vec![PlanAction {
            id: "a-001".into(),
            kind: ActionKind::SetLoginShell {
                shell: PathBuf::from("/opt/homebrew/bin/fish-dotfilesync-test"),
            },
            summary: "set login shell".into(),
            risk: Risk::High,
            reversible: true,
            backup: false,
        }],
        state_fingerprint: "fingerprint".into(),
    };
    let runner = MockCommandRunner::new();
    let now = || 50_u64;
    let ctx = context(
        &runner,
        tmp.path().join("backups"),
        tmp.path().join("run"),
        false,
        &now,
    );
    let err = apply_plan(&plan, &ctx, &mut |_| {}, &|| false).unwrap_err();
    let ApplyError::StepFailed {
        message, journal, ..
    } = err
    else {
        panic!("expected a failed step, got {err}");
    };
    assert!(
        message.contains("requires sudo; rerun with --allow-sudo"),
        "{message}"
    );
    assert_eq!(journal.steps[0].status, StepStatus::Failed);
    assert_eq!(
        journal.steps[0].error.as_deref(),
        Some("requires sudo; rerun with --allow-sudo")
    );
}
