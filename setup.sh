#!/usr/bin/env bash
# ============================================================================
# my-shell — Brewfile-driven macOS coding environment bootstrap
# ============================================================================
# Usage:
#   bash ./setup.sh
#   bash ./setup.sh --dry-run
#   bash ./setup.sh --skip-casks
#   bash ./setup.sh --skip-shell
#   bash ./setup.sh --skip-xcode
#   bash ./setup.sh --skip-fonts
#   bash ./setup.sh --skip-zed
# ============================================================================

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/log.sh
source "${REPO_ROOT}/lib/log.sh"
# shellcheck source=lib/brew.sh
source "${REPO_ROOT}/lib/brew.sh"
# shellcheck source=lib/fish.sh
source "${REPO_ROOT}/lib/fish.sh"
# shellcheck source=lib/node.sh
source "${REPO_ROOT}/lib/node.sh"
# shellcheck source=lib/xcode.sh
source "${REPO_ROOT}/lib/xcode.sh"
# shellcheck source=lib/fonts.sh
source "${REPO_ROOT}/lib/fonts.sh"
# shellcheck source=lib/zed.sh
source "${REPO_ROOT}/lib/zed.sh"
# shellcheck source=lib/models.sh
source "${REPO_ROOT}/lib/models.sh"

DRY_RUN=0
SKIP_CASKS=0
SKIP_SHELL=0
SKIP_XCODE=0
SKIP_FONTS=0
SKIP_ZED=0
SKIP_MODELS=0

usage() {
  cat <<'EOF'
Usage: bash ./setup.sh [options]

Options:
  --dry-run       Print what would happen without making changes
  --skip-casks    Skip Homebrew cask installs (CLI formulas only)
  --skip-shell    Do not change the login shell to Fish
  --skip-xcode    Skip latest Xcode install via xcodes
  --skip-fonts    Skip 0xProto (and other font cask) installs
  --skip-zed      Skip syncing Zed settings from zed/
  --skip-models   Skip LM Studio model downloads
  -h, --help      Show this help
EOF
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run) DRY_RUN=1 ;;
      --skip-casks) SKIP_CASKS=1 ;;
      --skip-shell) SKIP_SHELL=1 ;;
      --skip-xcode) SKIP_XCODE=1 ;;
      --skip-fonts) SKIP_FONTS=1 ;;
      --skip-zed) SKIP_ZED=1 ;;
      --skip-models) SKIP_MODELS=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "Unknown option: $1 (try --help)"
        ;;
    esac
    shift
  done
}

require_macos() {
  if [[ "$(uname)" != "Darwin" ]]; then
    die "This script is designed for macOS."
  fi
}

print_summary() {
  echo ""
  success "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  success "  Setup complete"
  success "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo ""
  info "Installed / configured:"
  echo "  • Homebrew + Brewfile packages"
  echo "  • Fish shell + Fisher plugins (Tide, done, bass)"
  echo "  • fnm + Node.js LTS + pnpm (Corepack)"
  echo "  • Latest Xcode via xcodes (unless --skip-xcode)"
  echo "  • 0xProto fonts via Homebrew (unless --skip-fonts)"
  echo "  • Fish config templates synced to ~/.config/fish"
  echo "  • Zed settings synced to ~/.config/zed (unless --skip-zed)"
  echo "  • LM Studio models (unless --skip-models)"
  echo ""
  info "Useful commands:"
  echo "  helpme           Show custom Fish commands"
  echo "  fisherup         Update Fisher plugins"
  echo "  reload           Reload Fish config"
  echo "  fnm list         List Node versions"
  echo "  xcodes install --latest --select"
  echo ""
  if [[ "$DRY_RUN" == "1" ]]; then
    warning "This was a dry run — no changes were applied."
  else
    warning "Restart your terminal or run 'exec fish' to load the new environment."
    info "If you previously used nvm, ~/.nvm was left alone. You can remove it after verifying fnm works."
    info "Select 0xProto in Ghostty / Zed (or other editors) if it is not already the active font."
  fi
  echo ""
}

main() {
  parse_args "$@"
  require_macos

  info "Starting my-shell setup..."
  [[ "$DRY_RUN" == "1" ]] && warning "Dry-run mode enabled"

  install_homebrew
  ensure_brew_on_path || die "Homebrew is required"

  run_brew_bundle "${REPO_ROOT}/Brewfile"
  ensure_rustup

  # Fonts after brew so --skip-casks still gets 0xProto; failures warn only.
  install_fonts

  sync_fish_templates
  install_fisher
  install_fisher_plugins

  sync_zed_config

  setup_fnm_node
  setup_pnpm

  setup_xcode

  set_fish_as_default_shell

  install_models

  print_summary
}

main "$@"
