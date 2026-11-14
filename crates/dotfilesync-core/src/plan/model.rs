use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const PLAN_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// Typed action. The executor maps each variant to a fixed command or filesystem operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionKind {
    BrewInstallFormula {
        name: String,
    },
    BrewInstallCask {
        name: String,
    },
    LinkFile {
        source: PathBuf,
        target: PathBuf,
    },
    CopyFile {
        source: PathBuf,
        target: PathBuf,
    },
    EnsureLocalOverride {
        target: PathBuf,
        template: Option<PathBuf>,
    },
    InstallFisher,
    FisherUpdate,
    SetLoginShell {
        shell: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAction {
    /// Stable index within this plan, `a-001` upward.
    pub id: String,
    pub kind: ActionKind,
    pub summary: String,
    pub risk: Risk,
    pub reversible: bool,
    /// The target exists and differs, so apply will back it up before replacing it.
    pub backup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub schema: u32,
    pub profile: Option<String>,
    pub manifest_path: PathBuf,
    pub home: PathBuf,
    pub repo_root: PathBuf,
    pub actions: Vec<PlanAction>,
    /// SHA-256 of the observed state the plan was computed from.
    pub state_fingerprint: String,
}

impl Plan {
    /// SHA-256 of the canonical JSON encoding of this plan.
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    /// `p-` plus the first 16 hex characters of [`Self::digest`].
    pub fn plan_id(&self) -> String {
        let digest = self.digest();
        format!("p-{}", &digest[..16])
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanInput {
    pub manifest: PathBuf,
    pub home: PathBuf,
    pub repo_root: Option<PathBuf>,
    /// `None` selects a profile automatically from `[select]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}
