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

# OpenCode loads plugins only from its own config dir, so the copy here is the
# source and this is the only thing that keeps the two from drifting.

# Install the OpenCode reporter.
sync-opencode:
    @mkdir -p ~/.config/opencode/plugin
    cp integrations/opencode.js ~/.config/opencode/plugin/agentbus.js
    @echo "synced into ~/.config/opencode/plugin/agentbus.js"

agy_hooks := env("HOME") / ".gemini/config/hooks.json"

# agy's global hooks file is ~/.gemini/config/hooks.json — established by
# tracing its syscalls, since its own documentation says only "your
# customization root". It probes four paths and opens that one. `.agents/` is a
# customization root for skills and rules; it is NOT read for hooks, at any
# level, which is a trap because putting hooks.json there looks right and fails
# silently.
#
# The binary path is substituted rather than left as `~`: agy's docs say it
# expands one, and it does not. That failure is silent too — the hook simply
# never runs and nothing anywhere says so.
#
# Top-level keys are hook names and are merged, so agentbus sits beside anyone
# else's entry, which is why this will not overwrite a file it did not write.

# Install the agy (Antigravity CLI) hooks, for every project at once.
sync-agy:
    @mkdir -p "$(dirname "{{ agy_hooks }}")"
    @if [ -e "{{ agy_hooks }}" ] && ! grep -q '"agentbus"' "{{ agy_hooks }}"; then \
        echo "{{ agy_hooks }} exists and has no agentbus entry."; \
        echo "Merge this in as a top-level key rather than replacing the file:"; \
        echo; sed "s|__AGENTBUS__|{{ prefix }}/bin/agentbus|g" integrations/agy-hooks.json; exit 1; \
     else \
        sed "s|__AGENTBUS__|{{ prefix }}/bin/agentbus|g" integrations/agy-hooks.json > "{{ agy_hooks }}"; \
        echo "synced into {{ agy_hooks }}"; \
     fi

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

# Open the system-wide interactive roster.
ui *ARGS:
    cargo run --release -- ui {{ ARGS }}

# Lint, warnings as errors.
check:
    cargo clippy --all-targets -- -D warnings

# Format.
fmt:
    cargo fmt
