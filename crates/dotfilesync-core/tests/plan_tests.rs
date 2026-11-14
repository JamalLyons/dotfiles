use dotfilesync_core::{run_plan, CommandOutput, MockCommandRunner, PlanInput, PLAN_SCHEMA};
use std::fs;
use std::path::PathBuf;

fn brew_runner(json: &str) -> MockCommandRunner {
    let brew = "/mock/bin/brew";
    MockCommandRunner::new()
        .with_executable("brew", Some(PathBuf::from(brew)))
        .with_response(
            brew,
            &["--version"],
            Ok(CommandOutput {
                status_code: 0,
                stdout: "Homebrew 4.2.0\n".into(),
                stderr: String::new(),
            }),
        )
        .with_response(
            brew,
            &["info", "--json=v2", "--installed"],
            Ok(CommandOutput {
                status_code: 0,
                stdout: json.into(),
                stderr: String::new(),
            }),
        )
}

fn write_fixture(root: &std::path::Path) -> PathBuf {
    let repo = root.join("repo");
    let home = root.join("home");
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::write(repo.join("config/app.conf"), "desired\n").unwrap();
    fs::write(home.join(".config/app.conf"), "original\n").unwrap();
    let manifest = repo.join("dotfilesync.toml");
    fs::write(
        &manifest,
        r#"
schema_version = 1

[package_managers.homebrew]
formulae = ["git"]

[[files]]
source = "config/app.conf"
target = "~/.config/app.conf"
mode = "copy"
"#,
    )
    .unwrap();
    manifest
}

#[test]
fn two_plan_runs_produce_identical_json() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = write_fixture(tmp.path());
    let home = tmp.path().join("home");
    let runner = brew_runner(
        r#"{"formulae":[{"name":"git","installed":[{"version":"2.45.0"}]}],"casks":[]}"#,
    );
    let input = PlanInput {
        manifest: manifest.clone(),
        home: home.clone(),
        repo_root: Some(tmp.path().join("repo")),
        profile: None,
    };
    let first = run_plan(&input, &runner, "test-host", &mut |_| {}, &|| false).unwrap();
    let second = run_plan(&input, &runner, "test-host", &mut |_| {}, &|| false).unwrap();
    let first_json = serde_json::to_vec(&first).unwrap();
    let second_json = serde_json::to_vec(&second).unwrap();
    assert_eq!(first_json, second_json);
    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.plan_id(), format!("p-{}", &first.digest()[..16]));
    assert_eq!(first.schema, PLAN_SCHEMA);
    assert!(!first.is_empty());
    assert!(first.actions.iter().any(|action| action.backup));
}
