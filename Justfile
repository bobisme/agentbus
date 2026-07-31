# agentbus — observe coding agents, publish normalised events.

set shell := ["bash", "-uc"]

prefix := env("HOME") / ".local"

_default:
    @just --list --unsorted

# A real copy rather than a symlink into target/: hook configurations reference
# this path absolutely, so it must survive a `cargo clean`. The cost is that a
# rebuild alone no longer updates them, which is why this recipe exists.

# Install where agent hooks can reach it.
install:
    cargo install --path . --root {{ prefix }} --force
    @echo "installed: {{ prefix }}/bin/agentbus"

# Install and enable the observer as a user service.
service: install
    mkdir -p ~/.config/systemd/user
    cp systemd/agentbus.service ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable --now agentbus.service
    systemctl --user status agentbus.service --no-pager | head -5

# Run in the foreground.
watch *ARGS:
    cargo run --release -- watch {{ ARGS }}

# Fold recent history once and print the resulting state.
snapshot *ARGS:
    cargo run --release --quiet -- scan --no-publish {{ ARGS }} | jq .

# Follow normalised events without publishing.
events *ARGS:
    cargo run --release --quiet -- events --no-publish {{ ARGS }}

# What subscribers currently see.
state:
    @jq . "${XDG_STATE_HOME:-$HOME/.local/state}/agentbus/snapshot.json" 2>/dev/null \
        || echo "no snapshot; is the observer running?"

# Lint, warnings as errors.
check:
    cargo clippy --all-targets -- -D warnings

# Format.
fmt:
    cargo fmt
