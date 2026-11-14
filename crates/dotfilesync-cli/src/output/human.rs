use dotfilesync_core::{
    ActionKind, BrewfileParseResult, ConfigDifference, DifferenceClassification, PackageDifference,
    PackageKind, Plan, Risk, RunJournal, ScanReport, StatusReport,
};

pub fn render_scan_report(report: &ScanReport) -> String {
    let mut out = String::new();
    out.push_str("=== dotfilesync scan ===\n\n");

    // Homebrew section
    out.push_str("HOMEBREW:\n");
    if report.homebrew.available {
        let version = report
            .homebrew
            .brew_version
            .as_deref()
            .unwrap_or("unknown version");
        let path = report
            .homebrew
            .brew_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "path unknown".to_string());
        out.push_str(&format!("  Status: Available ({version})\n"));
        out.push_str(&format!("  Binary: {path}\n"));
        out.push_str(&format!(
            "  Installed Formulae ({}): ",
            report.homebrew.formulae.len()
        ));
        if report.homebrew.formulae.is_empty() {
            out.push_str("none\n");
        } else {
            out.push('\n');
            for f in &report.homebrew.formulae {
                let ver = f.version.as_deref().unwrap_or("installed");
                out.push_str(&format!("    • {} ({ver})\n", f.name));
            }
        }

        out.push_str(&format!(
            "  Installed Casks ({}): ",
            report.homebrew.casks.len()
        ));
        if report.homebrew.casks.is_empty() {
            out.push_str("none\n");
        } else {
            out.push('\n');
            for c in &report.homebrew.casks {
                let ver = c.version.as_deref().unwrap_or("installed");
                out.push_str(&format!("    • {} ({ver})\n", c.name));
            }
        }
    } else {
        out.push_str("  Status: Unavailable (Homebrew not found)\n");
    }

    // Config files section
    out.push_str("\nCONFIG FILES (explicitly tracked):\n");
    if report.configs.is_empty() {
        out.push_str("  No tracked configuration files inspected.\n");
    } else {
        for file in &report.configs {
            let state_str = format!("{:?}", file.state);
            let sha_short = file
                .target_hash
                .as_deref()
                .map(|h| if h.len() >= 10 { &h[..10] } else { h })
                .unwrap_or("-");
            out.push_str(&format!(
                "  • Target: {}\n    State:  {} (SHA256: {})\n",
                file.target_path.display(),
                state_str,
                sha_short
            ));
            if let Some(diag) = &file.diagnostic {
                out.push_str(&format!("    Note:   {diag}\n"));
            }
        }
    }

    // Diagnostics section
    if !report.diagnostics.is_empty() {
        out.push_str("\nDIAGNOSTICS:\n");
        for d in &report.diagnostics {
            out.push_str(&format!("  [{:?}] {}\n", d.severity, d.message));
            if let Some(detail) = &d.detail {
                out.push_str(&format!("    Detail: {detail}\n"));
            }
        }
    }

    out
}

pub fn render_status_report(report: &StatusReport) -> String {
    let mut out = String::new();
    out.push_str("=== dotfilesync status ===\n\n");

    // Summary box
    out.push_str("SUMMARY:\n");
    out.push_str(&format!(
        "  Homebrew:  {}\n",
        if report.homebrew_available {
            report.brew_version.as_deref().unwrap_or("Available")
        } else {
            "Unavailable (diagnostic recorded)"
        }
    ));
    out.push_str(&format!(
        "  Packages:  {} matching, {} missing, {} unmanaged, {} unknown\n",
        report.summary.packages_matching,
        report.summary.packages_missing,
        report.summary.packages_unmanaged,
        report.summary.packages_unknown,
    ));
    out.push_str(&format!(
        "  Configs:   {} matching, {} changed (drift), {} missing, {} unknown, {} skipped\n",
        report.summary.configs_matching,
        report.summary.configs_changed,
        report.summary.configs_missing,
        report.summary.configs_unknown,
        report.summary.configs_skipped,
    ));

    // Missing packages
    let missing_pkgs: Vec<&PackageDifference> = report
        .package_differences
        .iter()
        .filter(|p| p.classification == DifferenceClassification::Missing)
        .collect();
    if !missing_pkgs.is_empty() {
        out.push_str(&format!("\nMISSING PACKAGES ({}):\n", missing_pkgs.len()));
        for p in missing_pkgs {
            let kind = match p.kind {
                PackageKind::Formula => "formula",
                PackageKind::Cask => "cask",
            };
            out.push_str(&format!("  ✗ [{kind}] {}\n", p.name));
        }
    }

    // Unknown packages (e.g. absent Homebrew)
    let unknown_pkgs: Vec<&PackageDifference> = report
        .package_differences
        .iter()
        .filter(|p| p.classification == DifferenceClassification::Unknown)
        .collect();
    if !unknown_pkgs.is_empty() {
        out.push_str(&format!(
            "\nUNKNOWN STATUS PACKAGES ({}):\n",
            unknown_pkgs.len()
        ));
        for p in unknown_pkgs {
            let kind = match p.kind {
                PackageKind::Formula => "formula",
                PackageKind::Cask => "cask",
            };
            let reason = p
                .diagnostic
                .as_deref()
                .unwrap_or("Status cannot be verified");
            out.push_str(&format!("  ? [{kind}] {}: {reason}\n", p.name));
        }
    }

    // Changed configs (drift)
    let changed_configs: Vec<&ConfigDifference> = report
        .config_differences
        .iter()
        .filter(|c| c.classification == DifferenceClassification::Changed)
        .collect();
    if !changed_configs.is_empty() {
        out.push_str(&format!(
            "\nCHANGED CONFIG FILES (DRIFT) ({}):\n",
            changed_configs.len()
        ));
        for c in changed_configs {
            let src_hash = c.source_hash.as_deref().unwrap_or("-");
            let tgt_hash = c.target_hash.as_deref().unwrap_or("-");
            let src_short = if src_hash.len() >= 10 {
                &src_hash[..10]
            } else {
                src_hash
            };
            let tgt_short = if tgt_hash.len() >= 10 {
                &tgt_hash[..10]
            } else {
                tgt_hash
            };
            out.push_str(&format!(
                "  ⚠ Target: {}\n    Source: {}\n    Repo Hash:   {}\n    Machine Hash: {}\n",
                c.target.display(),
                c.source.display(),
                src_short,
                tgt_short
            ));
            if let Some(diag) = &c.diagnostic {
                out.push_str(&format!("    Drift note:  {diag}\n"));
            }
        }
    }

    // Missing configs: separate ordinary missing managed configs from intentionally local overrides
    let mut ordinary_missing: Vec<&ConfigDifference> = Vec::new();
    let mut absent_overrides: Vec<&ConfigDifference> = Vec::new();

    for c in &report.config_differences {
        if c.classification == DifferenceClassification::Missing {
            if c.diagnostic
                .as_deref()
                .map(|d| d.contains("Machine-local override"))
                .unwrap_or(false)
            {
                absent_overrides.push(c);
            } else {
                ordinary_missing.push(c);
            }
        }
    }

    if !ordinary_missing.is_empty() {
        out.push_str(&format!(
            "\nMISSING CONFIG FILES ({}):\n",
            ordinary_missing.len()
        ));
        for c in ordinary_missing {
            out.push_str(&format!(
                "  ✗ Target: {}\n    Source: {}\n",
                c.target.display(),
                c.source.display()
            ));
        }
    }

    if !absent_overrides.is_empty() {
        out.push_str(&format!(
            "\nABSENT MACHINE-LOCAL OVERRIDES (not created on this machine) ({}):\n",
            absent_overrides.len()
        ));
        for c in absent_overrides {
            out.push_str(&format!(
                "  ○ Target: {}\n    Note:   Machine-local override is absent on this machine (intentionally local; no repository restoration required)\n",
                c.target.display()
            ));
        }
    }

    // Unknown configs (unreadable, missing repo source, symlink escaping root)
    let unknown_configs: Vec<&ConfigDifference> = report
        .config_differences
        .iter()
        .filter(|c| c.classification == DifferenceClassification::Unknown)
        .collect();
    if !unknown_configs.is_empty() {
        out.push_str(&format!(
            "\nUNKNOWN STATUS CONFIG FILES ({}):\n",
            unknown_configs.len()
        ));
        for c in unknown_configs {
            let reason = c
                .diagnostic
                .as_deref()
                .unwrap_or("Status cannot be verified");
            out.push_str(&format!(
                "  ? Target: {}\n    Reason: {reason}\n",
                c.target.display()
            ));
        }
    }

    // Skipped / machine-local configs
    let skipped_configs: Vec<&ConfigDifference> = report
        .config_differences
        .iter()
        .filter(|c| c.classification == DifferenceClassification::Skipped)
        .collect();
    if !skipped_configs.is_empty() {
        out.push_str(&format!(
            "\nSKIPPED / MACHINE-LOCAL CONFIGS ({}):\n",
            skipped_configs.len()
        ));
        for c in skipped_configs {
            let reason = c.diagnostic.as_deref().unwrap_or("Skipped");
            out.push_str(&format!(
                "  - Target: {}\n    Note:   {reason}\n",
                c.target.display()
            ));
        }
    }

    // Unmanaged packages (installed but not in manifest)
    let unmanaged_pkgs: Vec<&PackageDifference> = report
        .package_differences
        .iter()
        .filter(|p| p.classification == DifferenceClassification::Unmanaged)
        .collect();
    if !unmanaged_pkgs.is_empty() {
        out.push_str(&format!(
            "\nUNMANAGED PACKAGES (installed on system, not declared in manifest) ({}):\n",
            unmanaged_pkgs.len()
        ));
        for p in unmanaged_pkgs {
            let kind = match p.kind {
                PackageKind::Formula => "formula",
                PackageKind::Cask => "cask",
            };
            let ver = p.observed_version.as_deref().unwrap_or("installed");
            out.push_str(&format!("  • [{kind}] {} ({ver})\n", p.name));
        }
    }

    // Diagnostics
    if !report.diagnostics.is_empty() {
        out.push_str("\nDIAGNOSTICS:\n");
        for d in &report.diagnostics {
            out.push_str(&format!("  [{:?}] {}\n", d.severity, d.message));
            if let Some(detail) = &d.detail {
                out.push_str(&format!("    Detail: {detail}\n"));
            }
        }
    }

    out
}

pub fn render_import_result(result: &BrewfileParseResult, output_path: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("=== dotfilesync import-brewfile ===\n\n");

    out.push_str(&format!(
        "Imported {} formulae and {} casks statically.\n",
        result.formulae.len(),
        result.casks.len()
    ));

    if let Some(path) = output_path {
        out.push_str(&format!("Wrote manifest skeleton to: {path}\n"));
    }

    if !result.unsupported_constructs.is_empty() {
        out.push_str(&format!(
            "\nUNSUPPORTED OR DYNAMIC CONSTRUCTS ({}):\n",
            result.unsupported_constructs.len()
        ));
        for u in &result.unsupported_constructs {
            out.push_str(&format!(
                "  • Line {}: [{}] {}\n    Raw:    {}\n",
                u.line_number,
                u.construct_type,
                u.reason,
                u.raw_line.trim()
            ));
        }
    }

    if !result.diagnostics.is_empty() {
        out.push_str("\nDIAGNOSTICS:\n");
        for d in &result.diagnostics {
            out.push_str(&format!("  [{:?}] {}\n", d.severity, d.message));
        }
    }

    out
}

pub fn render_plan(plan: &Plan) -> String {
    let profile = plan.profile.as_deref().unwrap_or("base");
    let mut out = format!("Plan {} (profile {profile})\n", plan.plan_id());
    if plan.actions.is_empty() {
        out.push_str("No changes. The system matches the manifest.\n");
        return out;
    }
    let mut low = 0usize;
    let mut medium = 0usize;
    let mut high = 0usize;
    let mut reversible = 0usize;
    for action in &plan.actions {
        match action.risk {
            Risk::Low => low += 1,
            Risk::Medium => medium += 1,
            Risk::High => high += 1,
        }
        if action.reversible {
            reversible += 1;
        }
        let rev = if action.reversible { "rev" } else { "irrev" };
        out.push_str(&format!(
            "[{}] [{rev}] [{}] {}\n",
            risk_label(action.risk),
            action_tag(&action.kind),
            action.summary
        ));
    }
    let n = plan.actions.len();
    let noun = if n == 1 { "action" } else { "actions" };
    let irreversible = n - reversible;
    out.push_str(&format!(
        "\n{n} {noun}: low {low}, medium {medium}, high {high}; reversible {reversible}, irreversible {irreversible}\n"
    ));
    out
}

pub fn render_run(journal: &RunJournal) -> String {
    let mut out = format!("Run {} (plan {})\n", journal.run_id, journal.plan_digest);
    if journal.steps.is_empty() {
        out.push_str("No steps.\n");
        return out;
    }
    for step in &journal.steps {
        out.push_str(&format!("  {}  {:?}\n", step.action_id, step.status));
        if let Some(error) = &step.error {
            out.push_str(&format!("    {error}\n"));
        }
    }
    out
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

fn action_tag(kind: &ActionKind) -> &'static str {
    match kind {
        ActionKind::BrewInstallFormula { .. } | ActionKind::BrewInstallCask { .. } => "brew",
        ActionKind::LinkFile { .. } => "link",
        ActionKind::CopyFile { .. } => "copy",
        ActionKind::EnsureLocalOverride { .. } => "ensure",
        ActionKind::InstallFisher | ActionKind::FisherUpdate => "fisher",
        ActionKind::SetLoginShell { .. } => "shell",
    }
}
