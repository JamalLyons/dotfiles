//! Plan → approve → apply over IPC. The client that starts apply is dropped; a new client
//! reads the result. Tokens are single-use and bound to the reviewed digest.

mod common;

use common::*;
use dotfilesync_core::{ActionKind, Plan, PlanAction, PlanInput, Risk, PLAN_SCHEMA};
use dotfilesync_protocol::{Client, ClientError, ErrorCode, JobOutput, JobStatus};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn write_apply_brew(path: &Path) {
    let script = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo "Homebrew 4.3.0"
    exit 0
fi
if [ "$1" = "info" ]; then
    echo '{"formulae":[{"name":"git","installed":[{"version":"2.44.0"}]}],"casks":[{"token":"ghostty","version":"1.0.0"}]}'
    exit 0
fi
if [ "$1" = "install" ]; then
    sleep 1
    exit 0
fi
if [ "$1" = "list" ]; then
    exit 0
fi
exit 1
"#;
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn fixture(env: &Env) -> (PathBuf, PathBuf, PlanInput) {
    let repo = env.tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("config.txt"), "desired\n").unwrap();
    let manifest = repo.join("dotfilesync.toml");
    fs::write(
        &manifest,
        r#"schema_version = 1

[package_managers.homebrew]
formulae = ["ripgrep"]

[[files]]
source = "config.txt"
target = "~/.config/demo/config.txt"
"#,
    )
    .unwrap();
    let target_dir = env.home.join(".config/demo");
    fs::create_dir_all(&target_dir).unwrap();
    fs::write(target_dir.join("config.txt"), "old\n").unwrap();
    let input = PlanInput {
        manifest,
        home: env.home.clone(),
        repo_root: Some(repo),
        profile: None,
    };
    (target_dir.join("config.txt"), env.home.clone(), input)
}

fn wait_terminal(client: &mut Client, id: &str) -> dotfilesync_protocol::JobSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job = client.job_status(id).expect("job_status");
        if job.status.is_terminal() {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} did not finish: {job:?}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else if path.is_file() {
            count += 1;
        }
    }
    count
}

#[test]
fn apply_survives_client_disconnect_and_rejects_replay_and_tampered_digest() {
    let env = Env::new(Brew::Fast);
    write_apply_brew(&env.brew);
    let (target, _home, input) = fixture(&env);
    let _daemon = env.start();

    let mut client = env.client();
    let started = client.plan_create(input).unwrap();
    let planned = wait_terminal(&mut client, &started.job_id);
    assert_eq!(planned.status, JobStatus::Succeeded, "{planned:?}");
    let JobOutput::Plan { plan, plan_id } = client.job_result(&started.job_id).unwrap() else {
        panic!("plan job did not return a plan");
    };
    assert_eq!(plan_id, plan.plan_id());
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::BrewInstallFormula { .. })),
        "expected a brew install action: {plan:?}"
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::CopyFile { .. }) && action.backup),
        "expected a backup copy action: {plan:?}"
    );

    let tampered = client
        .plan_approve(&plan_id, "tampered-digest")
        .unwrap_err();
    match tampered {
        ClientError::Remote(error) => {
            assert_eq!(error.code, ErrorCode::ApprovalRejected);
            assert_eq!(error.details.unwrap()["reason"], "plan_mismatch");
        }
        other => panic!("unexpected {other:?}"),
    }

    let grant = client.plan_approve(&plan_id, &plan.digest()).unwrap();
    let job = client
        .plan_apply(&plan_id, &grant.token, false)
        .expect("apply accepted");
    let job_id = job.job_id.clone();
    let token = grant.token.clone();
    drop(client);

    let mut client = env.client();
    let mid = client.job_status(&job_id).unwrap();
    assert!(
        matches!(
            mid.status,
            JobStatus::Queued | JobStatus::Running | JobStatus::Succeeded
        ),
        "disconnected client must not cancel the job: {mid:?}"
    );
    let finished = wait_terminal(&mut client, &job_id);
    assert_eq!(finished.status, JobStatus::Succeeded, "{finished:?}");
    let JobOutput::Apply { journal } = client.job_result(&job_id).unwrap() else {
        panic!("apply job did not return a journal");
    };
    let view = client.plan_run_status(&journal.run_id).unwrap();
    assert_eq!(view.status, dotfilesync_protocol::RunViewStatus::Succeeded);
    assert_eq!(view.run_id, journal.run_id);
    assert!(
        view.steps
            .iter()
            .any(|step| step.status == dotfilesync_core::StepStatus::Done),
        "{view:?}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "desired\n");
    assert!(
        count_files(&env.layout.backups_dir) > 0,
        "backup of the previous target must exist"
    );
    let runs_mode = fs::metadata(&env.layout.runs_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let backups_mode = fs::metadata(&env.layout.backups_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(runs_mode, 0o700);
    assert_eq!(backups_mode, 0o700);

    let replay = client.plan_apply(&plan_id, &token, false).unwrap_err();
    match replay {
        ClientError::Remote(error) => {
            assert_eq!(error.code, ErrorCode::ApprovalRejected);
            assert_eq!(error.details.unwrap()["reason"], "already_used");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn unfinished_run_is_interrupted_after_restart_and_can_resume() {
    let env = Env::new(Brew::Fast);
    {
        let daemon = env.start();
        drop(daemon);
    }

    let repo = env.tmp.path().join("repo");
    fs::create_dir_all(repo.join("unused")).unwrap();
    fs::write(repo.join("config.txt"), "desired\n").unwrap();
    let manifest = repo.join("dotfilesync.toml");
    fs::write(
        &manifest,
        "schema_version = 1\n\n[[files]]\nsource = \"config.txt\"\ntarget = \"~/.config/demo/config.txt\"\n",
    )
    .unwrap();
    let target = env.home.join(".config/demo/config.txt");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old\n").unwrap();

    let plan = Plan {
        schema: PLAN_SCHEMA,
        profile: None,
        manifest_path: manifest,
        home: env.home.clone(),
        repo_root: repo.clone(),
        actions: vec![PlanAction {
            id: "a-001".into(),
            kind: ActionKind::CopyFile {
                source: repo.join("config.txt"),
                target: target.clone(),
            },
            summary: "copy config".into(),
            risk: Risk::Medium,
            reversible: true,
            backup: true,
        }],
        state_fingerprint: "fingerprint-for-resume".into(),
    };
    let run_id = "r-interrupted0001";
    let run_dir = env.layout.runs_dir.join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        run_dir.join("plan.json"),
        serde_json::to_vec(&plan).unwrap(),
    )
    .unwrap();
    let header = serde_json::json!({
        "event": "header",
        "run_id": run_id,
        "plan_digest": plan.digest(),
    });
    let step = serde_json::json!({
        "event": "step",
        "action_id": "a-001",
        "status": "running",
        "at_ms": 1
    });
    fs::write(run_dir.join("journal.jsonl"), format!("{header}\n{step}\n")).unwrap();
    let meta = serde_json::json!({
        "run_id": run_id,
        "plan_id": plan.plan_id(),
        "plan_digest": plan.digest(),
        "job_id": "j-old0001",
        "status": "running"
    });
    let meta_path = run_dir.join("meta.json");
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).unwrap();

    let _daemon = env.start();
    let mut client = env.client();
    let runs = client.plan_runs().unwrap();
    let found = runs
        .iter()
        .find(|run| run.run_id == run_id)
        .expect("run listed");
    assert_eq!(
        found.status,
        dotfilesync_protocol::RunViewStatus::Interrupted
    );
    let view = client.plan_run_status(run_id).unwrap();
    assert_eq!(
        view.status,
        dotfilesync_protocol::RunViewStatus::Interrupted
    );

    let grant = client
        .plan_approve(&view.plan_id, &view.plan_digest)
        .expect("reloaded plan can be approved");
    let job = client
        .plan_resume(run_id, &grant.token, false)
        .expect("interrupted run is resumable");
    let finished = wait_terminal(&mut client, &job.job_id);
    assert_eq!(finished.status, JobStatus::Succeeded, "{finished:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "desired\n");
}
