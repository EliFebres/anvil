#!/usr/bin/env bash
# One-time NUC bootstrap. Idempotent: run it again after a reboot or a pull.
#
#   ssh nuc 'bash -s' < deploy/nuc/bootstrap.sh
#
# Installs rustup, git, just and podman; clones the monorepo to ~/factory-repo;
# seeds ~/.factory/env and ~/.anvil/config.toml only when absent; builds anvil
# and factory in release mode into ~/.local/bin; runs anvil doctor.
set -euo pipefail

REPO_URL="${FACTORY_REPO_URL:-https://github.com/EliFebres/anvil.git}"
REPO_DIR="$HOME/factory-repo"
BIN_DIR="$HOME/.local/bin"
FACTORY_ENV="$HOME/.factory/env"
export PATH="$HOME/.cargo/bin:$BIN_DIR:$PATH"

log() { printf '\n== %s\n' "$*"; }

install_packages() {
    log "packages: git curl just podman"
    if command -v apt-get >/dev/null; then
        sudo apt-get update -qq
        sudo apt-get install -y -qq git curl build-essential pkg-config podman
        sudo apt-get install -y -qq just || true
    elif command -v dnf >/dev/null; then
        sudo dnf install -y git curl gcc make podman just
    elif command -v pacman >/dev/null; then
        sudo pacman -Sy --noconfirm --needed git curl base-devel podman just
    else
        echo "no supported package manager (apt-get, dnf, pacman); install git, curl, just and podman by hand" >&2
        exit 1
    fi
    if ! command -v just >/dev/null; then
        log "just: prebuilt binary into $BIN_DIR"
        mkdir -p "$BIN_DIR"
        curl --proto '=https' --tlsv1.2 -sSf https://just.systems/install.sh | bash -s -- --to "$BIN_DIR"
    fi
}

install_rust() {
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        log "rust: already installed ($("$HOME/.cargo/bin/cargo" --version))"
        return
    fi
    log "rust: rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
}

clone_repo() {
    if [ -d "$REPO_DIR/.git" ]; then
        log "repo: updating $REPO_DIR"
        git -C "$REPO_DIR" pull --ff-only
    else
        log "repo: cloning $REPO_URL to $REPO_DIR"
        git clone "$REPO_URL" "$REPO_DIR"
    fi
}

seed_env() {
    mkdir -p "$(dirname "$FACTORY_ENV")"
    if [ -f "$FACTORY_ENV" ]; then
        log "env: $FACTORY_ENV exists, kept"
    else
        log "env: writing $FACTORY_ENV from deploy/nuc/env.template (mode 600)"
        install -m 600 "$REPO_DIR/deploy/nuc/env.template" "$FACTORY_ENV"
    fi
    chmod 600 "$FACTORY_ENV"
}

build_binaries() {
    log "build: anvil and factory (release)"
    (cd "$REPO_DIR" && cargo build --release -p anvil-cli -p factory-cli)
    mkdir -p "$BIN_DIR"
    ln -sf "$REPO_DIR/target/release/anvil" "$BIN_DIR/anvil"
    ln -sf "$REPO_DIR/target/release/factory" "$BIN_DIR/factory"
}

seed_config() {
    # `anvil setup` writes ~/.anvil/config.toml with the same provider entries
    # only when it is absent, and never overwrites a filled-in value. It exits
    # 2 while placeholders remain, which is expected on a fresh machine.
    log "config: anvil setup"
    (cd "$REPO_DIR" && anvil setup) || true
}

install_packages
install_rust
clone_repo
seed_env
build_binaries
seed_config

log "doctor"
(cd "$REPO_DIR" && anvil doctor) || true

echo
echo "Open $FACTORY_ENV, paste the keys, then run: cd $REPO_DIR && anvil doctor"
