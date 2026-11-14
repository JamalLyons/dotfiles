//! In-memory plan cache and on-disk apply runs.
//!
//! Plans live only in memory (bounded). Approvals are separate and never written here.
//! Run directories under `state/runs` survive restart; `meta.json` records status but never
//! an approval token.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dotfilesync_core::{Plan, PlanInput, StepRecord, StepStatus};
use dotfilesync_protocol::{ErrorCode, ProtocolError, RunSummary, RunView, RunViewStatus};
use serde::{Deserialize, Serialize};

use crate::fsutil::{open_private_file, OpenMode};
use crate::log::Logger;

pub const MAX_PLANS: usize = 32;

#[derive(Debug, Clone)]
pub struct StoredPlan {
    pub plan: Plan,
    pub input: PlanInput,
    pub owner_uid: u32,
}

#[derive(Debug, Default)]
pub struct PlanStore {
    plans: Mutex<VecDeque<StoredPlan>>,
}

impl PlanStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, stored: StoredPlan) {
        let mut plans = self.plans.lock().unwrap_or_else(|p| p.into_inner());
        let id = stored.plan.plan_id();
        plans.retain(|existing| existing.plan.plan_id() != id);
        while plans.len() >= MAX_PLANS {
            plans.pop_front();
        }
        plans.push_back(stored);
    }

    pub fn get(&self, plan_id: &str) -> Option<StoredPlan> {
        self.plans
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|stored| stored.plan.plan_id() == plan_id)
            .cloned()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunMetaFile {
    run_id: String,
    plan_id: String,
    plan_digest: String,
    job_id: String,
    status: RunViewStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
}

#[derive(Debug, Clone)]
struct RunMeta {
    run_id: String,
    dir: PathBuf,
    plan_id: String,
    plan_digest: String,
    job_id: String,
    status: RunViewStatus,
    profile: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JournalLine {
    Header { run_id: String, plan_digest: String },
    Step(StepRecord),
}

pub struct RunIndex {
    log: Arc<Logger>,
    inner: Mutex<Vec<RunMeta>>,
}

impl RunIndex {
    /// Load run directories left by a previous process. A run still marked `running` becomes
    /// `interrupted` because this process did not start it.
    pub fn recover(runs_dir: &Path, log: Arc<Logger>, plans: &PlanStore, owner_uid: u32) -> Self {
        let index = Self {
            log,
            inner: Mutex::new(Vec::new()),
        };
        let entries = match fs::read_dir(runs_dir) {
            Ok(entries) => entries,
            Err(error) => {
                index
                    .log
                    .warn(&format!("could not read runs directory: {error}"));
                return index;
            }
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            match load_run(&dir) {
                Ok(mut meta) => {
                    if meta.status == RunViewStatus::Running {
                        meta.status = RunViewStatus::Interrupted;
                        if let Err(error) = write_meta(&meta) {
                            index.log.warn(&format!(
                                "could not mark run {} interrupted: {error}",
                                meta.run_id
                            ));
                        }
                    }
                    if let Some(stored) = stored_from_dir(&dir, &meta, owner_uid) {
                        plans.insert(stored);
                    }
                    index
                        .inner
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(meta);
                }
                Err(error) => index
                    .log
                    .warn(&format!("skipping run dir {}: {error}", dir.display())),
            }
        }
        index
    }

    /// Point an existing run at a new worker. Used by `plan.resume`.
    pub fn attach_job(&self, run_id: &str, job_id: &str) {
        let mut runs = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(meta) = runs.iter_mut().find(|meta| meta.run_id == run_id) else {
            return;
        };
        meta.job_id = job_id.to_string();
        meta.status = RunViewStatus::Running;
        if let Err(error) = write_meta(meta) {
            self.log.warn(&format!(
                "could not attach job {job_id} to run {run_id}: {error}"
            ));
        }
    }

    pub fn begin(&self, job_id: &str, dir: &Path, plan: &Plan, input: &PlanInput) {
        let meta = RunMeta {
            run_id: String::new(),
            dir: dir.to_path_buf(),
            plan_id: plan.plan_id(),
            plan_digest: plan.digest(),
            job_id: job_id.to_string(),
            status: RunViewStatus::Running,
            profile: input.profile.clone(),
        };
        if let Err(error) = write_meta(&meta) {
            self.log.warn(&format!(
                "could not write run metadata for job {job_id}: {error}"
            ));
        }
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(meta);
    }

    pub fn note_run_id(&self, job_id: &str, run_id: &str) {
        let mut runs = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(meta) = runs.iter_mut().find(|meta| meta.job_id == job_id) else {
            return;
        };
        if meta.run_id == run_id {
            return;
        }
        meta.run_id = run_id.to_string();
        if let Err(error) = write_meta(meta) {
            self.log.warn(&format!(
                "could not record run id for job {job_id}: {error}"
            ));
        }
    }

    /// Rename a pending directory to `<runs>/<run_id>` and record the terminal status.
    pub fn finish(
        &self,
        job_id: &str,
        status: RunViewStatus,
        run_id: Option<&str>,
    ) -> Option<PathBuf> {
        let mut runs = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let meta = runs.iter_mut().find(|meta| meta.job_id == job_id)?;
        if let Some(run_id) = run_id {
            if !run_id.is_empty() {
                meta.run_id = run_id.to_string();
            }
        }
        if !meta.run_id.is_empty() {
            if let Some(parent) = meta.dir.parent() {
                let dest = parent.join(&meta.run_id);
                if meta.dir != dest && !dest.exists() {
                    match fs::rename(&meta.dir, &dest) {
                        Ok(()) => meta.dir = dest,
                        Err(error) => self.log.warn(&format!(
                            "could not rename run dir to {}: {error}",
                            dest.display()
                        )),
                    }
                }
            }
        }
        meta.status = status;
        if let Err(error) = write_meta(meta) {
            self.log.warn(&format!(
                "could not update run metadata for job {job_id}: {error}"
            ));
        }
        Some(meta.dir.clone())
    }

    pub fn dir_for_run(&self, run_id: &str) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|meta| meta.run_id == run_id)
            .map(|meta| meta.dir.clone())
    }

    pub fn list(&self) -> Vec<RunSummary> {
        let mut runs: Vec<RunSummary> = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|meta| !meta.run_id.is_empty())
            .map(|meta| RunSummary {
                run_id: meta.run_id.clone(),
                plan_id: meta.plan_id.clone(),
                plan_digest: meta.plan_digest.clone(),
                job_id: meta.job_id.clone(),
                status: meta.status,
            })
            .collect();
        runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        runs
    }

    pub fn get(&self, run_id: &str) -> Result<RunView, ProtocolError> {
        let meta = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|meta| meta.run_id == run_id)
            .cloned()
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::PlanNotFound,
                    format!("no run with id '{run_id}'"),
                )
            })?;
        let (_id, plan_digest, steps) = read_journal(&meta.dir).unwrap_or((
            meta.run_id.clone(),
            meta.plan_digest.clone(),
            Vec::new(),
        ));
        Ok(RunView {
            run_id: meta.run_id,
            plan_digest,
            steps,
            status: meta.status,
            plan_id: meta.plan_id,
            job_id: meta.job_id,
        })
    }

    pub fn is_active(&self, run_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .any(|meta| meta.run_id == run_id && meta.status == RunViewStatus::Running)
    }
}

pub fn read_run_id(dir: &Path) -> Option<String> {
    read_journal(dir)
        .ok()
        .map(|(run_id, _, _)| run_id)
        .filter(|id| !id.is_empty())
}

fn stored_from_dir(dir: &Path, meta: &RunMeta, owner_uid: u32) -> Option<StoredPlan> {
    let bytes = fs::read(dir.join("plan.json")).ok()?;
    let plan: Plan = serde_json::from_slice(&bytes).ok()?;
    if plan.digest() != meta.plan_digest && !meta.plan_digest.is_empty() {
        return None;
    }
    let input = PlanInput {
        manifest: plan.manifest_path.clone(),
        home: plan.home.clone(),
        repo_root: Some(plan.repo_root.clone()),
        profile: meta.profile.clone().or_else(|| plan.profile.clone()),
    };
    Some(StoredPlan {
        plan,
        input,
        owner_uid,
    })
}

fn load_run(dir: &Path) -> Result<RunMeta, String> {
    let meta_path = dir.join("meta.json");
    if meta_path.is_file() {
        let bytes = fs::read(&meta_path)
            .map_err(|error| format!("read {}: {error}", meta_path.display()))?;
        let file: RunMetaFile = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode {}: {error}", meta_path.display()))?;
        let mut meta = RunMeta {
            run_id: file.run_id,
            dir: dir.to_path_buf(),
            plan_id: file.plan_id,
            plan_digest: file.plan_digest,
            job_id: file.job_id,
            status: file.status,
            profile: file.profile,
        };
        if meta.run_id.is_empty() {
            if let Some(id) = read_run_id(dir) {
                meta.run_id = id;
            }
        }
        return Ok(meta);
    }
    let (run_id, plan_digest, steps) = read_journal(dir)?;
    let plan_id = fs::read(dir.join("plan.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Plan>(&bytes).ok())
        .map(|plan| plan.plan_id())
        .unwrap_or_default();
    Ok(RunMeta {
        run_id,
        dir: dir.to_path_buf(),
        plan_id,
        plan_digest,
        job_id: String::new(),
        status: status_from_steps(&steps, false),
        profile: None,
    })
}

fn status_from_steps(steps: &[StepRecord], live: bool) -> RunViewStatus {
    if steps.iter().any(|step| step.status == StepStatus::Failed) {
        return RunViewStatus::Failed;
    }
    let mut latest: Vec<(String, StepStatus)> = Vec::new();
    for step in steps {
        if let Some(found) = latest.iter_mut().find(|(id, _)| id == &step.action_id) {
            found.1 = step.status;
        } else {
            latest.push((step.action_id.clone(), step.status));
        }
    }
    let finished = !latest.is_empty()
        && latest
            .iter()
            .all(|(_, status)| matches!(status, StepStatus::Done | StepStatus::Skipped));
    if finished {
        RunViewStatus::Succeeded
    } else if live {
        RunViewStatus::Running
    } else {
        RunViewStatus::Interrupted
    }
}

fn read_journal(dir: &Path) -> Result<(String, String, Vec<StepRecord>), String> {
    let path = dir.join("journal.jsonl");
    let file =
        fs::File::open(&path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut run_id = None;
    let mut plan_digest = None;
    let mut steps = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| format!("read journal: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: JournalLine = serde_json::from_str(&line)
            .map_err(|error| format!("journal line {}: {error}", index + 1))?;
        match parsed {
            JournalLine::Header {
                run_id: id,
                plan_digest: digest,
            } => {
                run_id = Some(id);
                plan_digest = Some(digest);
            }
            JournalLine::Step(step) => steps.push(step),
        }
    }
    Ok((
        run_id.ok_or_else(|| "journal is missing a header".to_string())?,
        plan_digest.ok_or_else(|| "journal is missing a plan digest".to_string())?,
        steps,
    ))
}

fn write_meta(meta: &RunMeta) -> std::io::Result<()> {
    if meta.dir.as_os_str().is_empty() {
        return Ok(());
    }
    let file = RunMetaFile {
        run_id: meta.run_id.clone(),
        plan_id: meta.plan_id.clone(),
        plan_digest: meta.plan_digest.clone(),
        job_id: meta.job_id.clone(),
        status: meta.status,
        profile: meta.profile.clone(),
    };
    let bytes = serde_json::to_vec(&file)
        .map_err(|error| std::io::Error::other(format!("encode run metadata: {error}")))?;
    let mut out = open_private_file(&meta.dir.join("meta.json"), OpenMode::Truncate)?;
    use std::io::Write;
    out.write_all(&bytes)?;
    out.sync_all()
}
