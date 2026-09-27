#!/usr/bin/env bash
# install.sh — Install orchestra (TUI + pi + OpenClaw + SkyPilot CLI).
#
# One-liner (public repo):
#   curl -fsSL https://raw.githubusercontent.com/rohansonecha/orchestra/main/install.sh | bash
#
# One-liner (private repo, needs gh auth):
#   curl -fsSL -H "Authorization: Bearer $(gh auth token)" \
#     https://raw.githubusercontent.com/rohansonecha/orchestra/main/install.sh | bash
#
# From a local clone:
#   bash install.sh
#
# Re-running is safe — it updates the repo, rebuilds the TUI, and upgrades
# CLI deps. Config in private/ and ~/.orchestra is never overwritten.
#
# Env knobs:
#   ORCHESTRA_HOME     clone location        (default: ~/orchestra)
#   ORCHESTRA_BIN_DIR  where to link `orchestra` (default: /usr/local/bin or ~/.local/bin)
#   ORCHESTRA_REPO     git repo slug         (default: rohansonecha/orchestra)
#   SKIP_DEPS=1        skip dependency installs (just clone + build + link)

set -euo pipefail

# --------------------------------------------------------------------------
# Pretty output
# --------------------------------------------------------------------------
if [ -t 1 ]; then
  BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'
  YELLOW=$'\033[33m'; CYAN=$'\033[36m'; RESET=$'\033[0m'
else
  BOLD=""; DIM=""; RED=""; GREEN=""; YELLOW=""; CYAN=""; RESET=""
fi

step()  { printf "\n${BOLD}${CYAN}==>${RESET} ${BOLD}%s${RESET}\n" "$*"; }
info()  { printf "    %s\n" "$*"; }
ok()    { printf "    ${GREEN}✓${RESET} %s\n" "$*"; }
warn()  { printf "    ${YELLOW}!${RESET} %s\n" "$*" >&2; }
die()   { printf "\n${RED}error:${RESET} %s\n" "$*" >&2; exit 1; }

need_cmd() { command -v "$1" >/dev/null 2>&1; }

# --------------------------------------------------------------------------
# Config
# --------------------------------------------------------------------------
ORCHESTRA_HOME="${ORCHESTRA_HOME:-$HOME/orchestra}"
ORCHESTRA_REPO="${ORCHESTRA_REPO:-rohansonecha/orchestra}"
SKIP_DEPS="${SKIP_DEPS:-0}"

OS="$(uname -s)"   # Darwin | Linux
ARCH="$(uname -m)" # arm64 | x86_64

printf "${BOLD}orchestra installer${RESET} ${DIM}(%s/%s)${RESET}\n" "$OS" "$ARCH"

# --------------------------------------------------------------------------
# Are we running from inside a clone, or standalone (curl | bash)?
# --------------------------------------------------------------------------
SCRIPT_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ "${BASH_SOURCE[0]}" != "bash" ] \
   && [ "${BASH_SOURCE[0]}" != "-bash" ] && [ -e "${BASH_SOURCE[0]}" ]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

IN_CLONE=0
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/tui/Cargo.toml" ]; then
  IN_CLONE=1
  ORCHESTRA_HOME="$SCRIPT_DIR"
  info "Running from local clone: $ORCHESTRA_HOME"
fi

# --------------------------------------------------------------------------
# Dependencies
# --------------------------------------------------------------------------
install_node() {
  if [ "$OS" = "Darwin" ]; then
    if need_cmd brew; then
      brew install node
    else
      die "Node.js missing and Homebrew not found. Install Node 24+: https://nodejs.org"
    fi
  else
    curl -fsSL https://deb.nodesource.com/setup_24.x | sudo -E bash -
    sudo apt-get install -y nodejs
  fi
}

# npm -g, using sudo only when needed AND non-interactive.
npm_global() {
  if npm install -g "$@" 2>/dev/null; then
    return 0
  fi
  if sudo -n true 2>/dev/null; then
    sudo npm install -g "$@"
  else
    return 1
  fi
}

# OpenClaw requires Node >=24.16 <25 || >=26.1.
node_ok_for_openclaw() {
  local major minor
  major="$(node -p 'process.versions.node.split(".")[0]')"
  minor="$(node -p 'process.versions.node.split(".")[1]')"
  { [ "$major" = "24" ] && [ "$minor" -ge 16 ]; } || [ "$major" -ge 26 ]
}

install_rust() {
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
}

if [ "$SKIP_DEPS" != "1" ]; then
  step "Checking dependencies"

  need_cmd git || die "git is required. On macOS: xcode-select --install"
  ok "git"

  if ! need_cmd node; then
    info "Installing Node.js 22..."
    install_node
  fi
  ok "node $(node --version)"

  if ! need_cmd tmux; then
    info "Installing tmux..."
    if [ "$OS" = "Darwin" ]; then
      need_cmd brew && brew install tmux || die "tmux missing and no Homebrew. Install tmux manually."
    else
      sudo apt-get install -y tmux
    fi
  fi
  ok "tmux"

  if ! need_cmd cargo; then
    info "Installing Rust (rustup)..."
    install_rust
  else
    # shellcheck disable=SC1091
    [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env" || true
  fi
  ok "rust $(rustc --version | awk '{print $2}')"

  if ! need_cmd pi; then
    info "Installing pi coding agent..."
    npm_global --ignore-scripts @earendil-works/pi-coding-agent \
      || die "pi install failed (npm global not writable and no passwordless sudo)"
  fi
  ok "pi"

  if ! need_cmd openclaw; then
    if node_ok_for_openclaw; then
      info "Installing OpenClaw..."
      npm_global openclaw@latest \
        || warn "OpenClaw install failed — install manually: npm install -g openclaw@latest"
    else
      warn "OpenClaw requires Node >=24.16 (you have $(node --version)) — skipping. Upgrade Node, then: npm install -g openclaw@latest"
    fi
  fi
  need_cmd openclaw && ok "openclaw" || true

  if ! need_cmd sky; then
    info "Installing SkyPilot CLI..."
    if need_cmd pip3; then
      pip3 install --upgrade "skypilot[kubernetes]"
    else
      die "pip3 not found. Install Python 3 first."
    fi
  fi
  ok "sky"
else
  step "Skipping dependency installs (SKIP_DEPS=1)"
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env" || true
fi

# --------------------------------------------------------------------------
# Clone or update the repo
# --------------------------------------------------------------------------
if [ "$IN_CLONE" = "0" ]; then
  step "Fetching orchestra repo → $ORCHESTRA_HOME"

  # Pick an authenticated clone URL for private repos.
  clone_url="https://github.com/${ORCHESTRA_REPO}.git"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    clone_url="https://x-access-token:${GITHUB_TOKEN}@github.com/${ORCHESTRA_REPO}.git"
  elif need_cmd gh && gh auth token >/dev/null 2>&1; then
    clone_url="https://x-access-token:$(gh auth token)@github.com/${ORCHESTRA_REPO}.git"
  elif [ -e "$HOME/.ssh/id_ed25519" ] || [ -e "$HOME/.ssh/id_rsa" ]; then
    clone_url="git@github.com:${ORCHESTRA_REPO}.git"
  fi

  if [ -d "$ORCHESTRA_HOME/.git" ]; then
    info "Repo exists — pulling latest"
    git -C "$ORCHESTRA_HOME" pull --rebase --autostash || warn "git pull failed; continuing with existing checkout"
  else
    git clone "$clone_url" "$ORCHESTRA_HOME" || die "Clone failed. For a private repo, set GITHUB_TOKEN or run 'gh auth login'."
  fi
  ok "$(git -C "$ORCHESTRA_HOME" log -1 --format='%h %s')"
fi

# --------------------------------------------------------------------------
# Build the TUI
# --------------------------------------------------------------------------
step "Building orchestra"
cargo build --release --manifest-path "$ORCHESTRA_HOME/tui/Cargo.toml"
TUI_BIN="$ORCHESTRA_HOME/tui/target/release/orchestra"
[ -x "$TUI_BIN" ] || die "Build succeeded but $TUI_BIN missing?"
ok "built $TUI_BIN"

# --------------------------------------------------------------------------
# Link onto PATH
# --------------------------------------------------------------------------
step "Installing 'orchestra' command"
BIN_DIR="${ORCHESTRA_BIN_DIR:-}"
if [ -z "$BIN_DIR" ]; then
  if [ -w /usr/local/bin ]; then
    BIN_DIR="/usr/local/bin"
  elif [ -w /opt/homebrew/bin ]; then
    BIN_DIR="/opt/homebrew/bin"
  elif sudo -n true 2>/dev/null; then
    BIN_DIR="/usr/local/bin"
  else
    BIN_DIR="$HOME/.local/bin"
  fi
fi
mkdir -p "$BIN_DIR"
if [ "$BIN_DIR" = "/usr/local/bin" ] && [ ! -w "$BIN_DIR" ]; then
  sudo ln -sf "$TUI_BIN" "$BIN_DIR/orchestra"
else
  ln -sf "$TUI_BIN" "$BIN_DIR/orchestra"
fi
ok "linked $BIN_DIR/orchestra → $TUI_BIN"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH — add it to your shell profile" ;;
esac

# --------------------------------------------------------------------------
# Bootstrap config (never overwrite existing)
# --------------------------------------------------------------------------
step "Bootstrapping config"
mkdir -p "$HOME/.orchestra"

if [ ! -d "$ORCHESTRA_HOME/private" ] && [ -d "$ORCHESTRA_HOME/private.example" ]; then
  cp -R "$ORCHESTRA_HOME/private.example" "$ORCHESTRA_HOME/private"
  [ -f "$ORCHESTRA_HOME/private/env.example" ] && mv "$ORCHESTRA_HOME/private/env.example" "$ORCHESTRA_HOME/private/env"
  ok "created $ORCHESTRA_HOME/private/ from template"
else
  ok "private/ already exists — left untouched"
fi

[ -L "$HOME/.orchestra/skills" ] || ln -sfn "$ORCHESTRA_HOME/skills" "$HOME/.orchestra/skills"
ok "~/.orchestra/skills → $ORCHESTRA_HOME/skills"

# --------------------------------------------------------------------------
# Done
# --------------------------------------------------------------------------
step "Verifying"
"$BIN_DIR/orchestra" version >/dev/null 2>&1 \
  || warn "binary exists but 'orchestra version' failed"
ok "orchestra installed"

cat <<EOF

${BOLD}${GREEN}Done!${RESET} Next steps:

  1. Fill in secrets:        ${CYAN}\$EDITOR $ORCHESTRA_HOME/private/env${RESET}
  2. Add pi model config:    ${CYAN}cp $ORCHESTRA_HOME/pi/models.json.example $ORCHESTRA_HOME/private/models.json${RESET} ${DIM}(edit host)${RESET}
  3. Launch the main box:    ${CYAN}source $ORCHESTRA_HOME/private/env && sky launch --infra "\$SKY_INFRA" -c main-box $ORCHESTRA_HOME/skypilot/main-box.yaml${RESET}
  4. Run the TUI:            ${CYAN}orchestra${RESET}

${DIM}Docs: https://github.com/${ORCHESTRA_REPO}${RESET}
EOF
