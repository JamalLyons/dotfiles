# Plan and apply

`plan` computes the difference between a resolved manifest and this machine. `apply` performs that plan through the daemon, one typed action at a time. A machine that already matches the manifest produces an **empty plan**.

`setup.sh` and `lib/` stay in the repo until the parity matrix below is filled in. They are still a valid way to bootstrap a Mac.

Schema, profiles, and merge order: [manifest.md](manifest.md).

## Commands

```bash
dotfilesync plan [--manifest] [--profile] [--json]
dotfilesync apply [--profile] [--yes] [--allow-sudo] [--dry-run]
dotfilesync apply --resume <run_id>
dotfilesync runs [list|show <id>]
```

`--profile` selects a named profile. Omit it and selection is automatic (`select.hostnames`, then `select.default`, then the base layer). Already-installed packages are left alone: dotfilesync never upgrades Homebrew packages (use Vorssaint or `brew upgrade` for that).

`plan` may run in-process. `apply` requires the daemon (`dotfilesync service start`). The journal and the locks live in daemon state.

## Workflow

1. **Plan.** `dotfilesync plan` loads the manifest, resolves the profile, and emits only what differs. Action order is fixed: formula installs, cask installs, files sorted by target path, local overrides, Fisher, login shell. Ids are `a-001` upward in that order. The plan id is `p-` plus the first 16 hex characters of the plan digest.
2. **Review.** Human output is one line per action: `[risk] [rev|irrev] [copy|link] summary`, then totals. `--json` prints the plan struct. Run `plan` twice on an unchanged machine and the JSON body matches.
3. **Approve.** `apply` prints the plan. On a TTY, without `--yes`, it asks `Apply N actions? [y/N]`. A non-TTY without `--yes` does not apply (dry-run, exit 0, with a note). `--dry-run` never applies.
4. **Apply.** After approval the daemon takes the Homebrew and home locks and runs the plan. Progress is a step at a time. Ctrl-C detaches the CLI and leaves the job running; follow it with `dotfilesync jobs follow <id>`.
5. **Resume.** `dotfilesync apply --resume <run_id>` needs a new approval. The run's plan is reused. Steps already `Done` are skipped. `Failed`, `Running`, and `Pending` are retried. The run stops at the first failure; later steps stay `Pending`. `dotfilesync runs list` and `dotfilesync runs show <id>` read the journal. After a daemon restart, a run with unfinished steps is `interrupted` and can be resumed.

## Safety

- `scan` and `status` stay read-only. They do not install, copy, or change the login shell.
- `--dry-run` and a non-TTY `apply` without `--yes` do not change the machine.
- Apply does not call `sudo` on its own. Setting the login shell appends to `/etc/shells` only when the shell is missing from that file **and** you passed `--allow-sudo`. Otherwise that step fails with `requires sudo; rerun with --allow-sudo`.
- Before a copy or link replaces an existing target, the file is moved under the daemon backups directory for that run: `<data>/backups/<run_id>/<path relative to home>.backup.YYYYmmdd_HHMMSS`. The default data directory is `~/Library/Application Support/dotfilesync` (override with `--data-dir` or `DOTFILESYNC_DATA_DIR`). The journal records that path. User files are not deleted in place: a symlink replacement moves the original into the backup, then creates the link. Parents are created mode `0755`. Writes go to a temp file in the target directory, then rename.
- The journal is `<data>/state/runs/<run_id>/journal.jsonl` plus a `plan.json` copy, fsynced after each step transition.
- `local.fish` and any path in `overrides` or `exclusions` are never a copy or a link. A missing override becomes `EnsureLocalOverride` (create if absent, including from the repo template when that template is a real file). If the file exists, it is left byte-for-byte alone.
- Apply refuses a copy or link whose target is an override or matches an exclusion, even if a plan named one.
- Package actions never uninstall. A name removed from the manifest is simply not installed next time.

`font-0xproto` is a Homebrew cask in the manifest. The planner installs it with `brew install --cask font-0xproto`. There is no separate font action. Unlike `setup.sh --skip-casks`, which still installs 0xProto, skipping casks is not a plan flag: drop the cask from the resolved manifest if you do not want it.

## Reversible and irreversible actions

Risk and reversibility are set by the planner. `backup` is true when a copy or link will replace a target that already exists and differs.

| Action | Risk | Reversible | What apply does |
|---|---|---|---|
| `brew install <formula>` | low | no | Installs the formula. Never auto-uninstalls. |
| `brew install --cask <cask>` | medium | no | Installs the cask. Never auto-uninstalls. |
| Copy or link, target absent | low | yes | Creates the file. Undo is removing it. |
| Copy or link, target exists and differs | medium | yes | Backs up first, then replaces. Undo is restoring the backup. |
| Ensure local override (missing only) | low | yes | Creates the file when it is absent. Does not rewrite an existing one. |
| Install Fisher, then `fisher update` | medium | no | Emitted only when `setup.fisher` is true **and** `~/.config/fish/functions/fisher.fish` is absent. Both actions are emitted together. Fixed `fish -c` scripts, not caller-supplied shell. |
| Set login shell | high | yes | `chsh -s <brew prefix>/bin/<login_shell>`. The previous shell is stored on the step. Undo is `chsh` back to that path. `sudo` only with `--allow-sudo`, and only to append a shell that is not already in `/etc/shells`. |

## setup.sh parity

| `setup.sh` step | Plan / apply | Gap |
|---|---|---|
| Install Homebrew if missing | Not ported | Discovery only. Apply assumes `brew` is already on the machine. |
| `brew bundle` formulae | `BrewInstallFormula` | Missing formulae only. No uninstall. |
| `brew bundle` casks | `BrewInstallCask` | Missing casks only. |
| `rustup` toolchain bootstrap (`ensure_rustup`) | The `rustup` formula, if declared | No `rustup-init` / default-toolchain step beyond the formula. |
| 0xProto via `font-0xproto`, including after `--skip-casks` | The `font-0xproto` cask | No separate font step and no `--skip-fonts`. A cask-only skip in setup.sh still installs the font; plan/apply will not, unless the cask remains in the resolved manifest. |
| Sync `fish/` templates (copy, backup when different) | `CopyFile` for each tracked fish file whose bytes differ | No interactive overwrite prompt. The reviewed plan is the confirmation. Backup goes under the daemon backups dir, and the journal keeps the `name.backup.YYYYmmdd_HHMMSS` stamp. |
| `fish/conf.d/local.fish` create-if-missing | `EnsureLocalOverride` | Same rule: never overwrite. Also true for every `overrides` entry. |
| Install Fisher when `fisher.fish` is absent | `InstallFisher` | Same condition. Requires `setup.fisher = true`. |
| `fisher update` from `fish_plugins` on later runs | `FisherUpdate` | Only paired with install, when `fisher.fish` is absent. A later run does not refresh plugins if Fisher is already installed. |
| Sync Zed settings, prompt before overwrite | `CopyFile` of `zed/settings.json` | Backup then replace. No per-file `[y/N]`. |
| fnm, Node LTS, pnpm via Corepack | Not ported | Issue #5. `fnm` can still be installed as a formula, and `fish/conf.d/fnm.fish` is a tracked file. |
| Latest Xcode via `xcodes` | Not ported | Issue #5. The `xcodes` formula can be installed; that does not download Xcode. |
| Fish as login shell (`chsh`, `sudo` to edit `/etc/shells`) | `SetLoginShell` | Plan uses `<brew --prefix>/bin/fish`. Setup uses `command -v fish`. Apply never runs `sudo` unless `--allow-sudo` is set; setup.sh prompts via `sudo` itself. |
| LM Studio models (`lms get`) | Not ported | Issue #5. The `lm-studio` cask can still be installed. |

`bash ./setup.sh` remains the path for the rows marked not ported.
