#!/bin/bash
# tests/upgrade.sh — Upgrade safety tests.
#
# Verifies that upgrading the TUI binary (control plane) does NOT
# break live sessions (data plane). The TUI is just a control process —
# sessions live in tmux, which is independent. Upgrading = kill TUI,
# rebuild, restart. Sessions should survive.
#
# Tests upgrade at different points:
#   1. After dispatch, before attach (session ready)
#   2. While session is still initializing
#   3. After detach (verify re-attach works)
#
# Note: tui-use can't trigger tmux root-table key bindings (Left arrow,
# Ctrl+C) because tmux attach reads from /dev/tty, not the PTY that
# tui-use writes to. We use `tmux detach-client` directly to test detach.
#
# Run on the test box (via tests/run-on-staging.sh):
#   bash ~/orchestra/tests/upgrade.sh
# Do NOT run directly on main-box — it kills all tmux sessions.

set -euo pipefail

ORCHESTRA_DIR="$HOME/orchestra"

PASS=0
FAIL=0

assert_contains() {
    local desc="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc (expected '$needle' in output)"
        FAIL=$((FAIL + 1))
    fi
}

assert_not_contains() {
    local desc="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  FAIL: $desc (did not expect '$needle' in output)"
        FAIL=$((FAIL + 1))
    else
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    fi
}

cleanup() {
    tui-use kill 2>/dev/null || true
    pkill -f orchestra-tui 2>/dev/null || true
    tmux kill-server 2>/dev/null || true
    sleep 1
    rm -rf ~/.orchestra/sessions/* ~/orchestra/worktrees/* 2>/dev/null || true
}

# Simulate an upgrade: kill TUI, rebuild, restart.
# tmux sessions survive because they're independent processes.
# This is the control/data plane separation — the TUI (control plane)
# can be killed and rebuilt without affecting tmux (data plane).
do_upgrade() {
    echo "  [upgrade] Killing TUI process..."
    pkill -f orchestra-tui 2>/dev/null || true
    pkill -f tui-use 2>/dev/null || true
    sleep 1

    echo "  [upgrade] Rebuilding binary..."
    cd "$ORCHESTRA_DIR" && git pull origin main 2>&1 | tail -1
    cd "$ORCHESTRA_DIR/tui" && source ~/.cargo/env && cargo build --release 2>&1 | tail -1

    echo "  [upgrade] Restarting TUI..."
    tui-use start orchestra
    sleep 2
}

wait_for_ready() {
    local timeout="${1:-30}"
    local elapsed=0
    while [ "$elapsed" -lt "$((timeout * 2))" ]; do
        if ls ~/.orchestra/sessions/*.ready 2>/dev/null | grep -q .; then
            return 0
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done
    return 1
}

trap cleanup EXIT

echo "=== Upgrade Safety Tests ==="
echo ""
echo "Control plane (TUI binary) vs data plane (tmux sessions)."
echo "Upgrading the control plane must not affect the data plane."
echo ""

# ---------------------------------------------------------------------------
# Test 1: Upgrade after dispatch, before attach
# ---------------------------------------------------------------------------
echo "--- Test 1: Upgrade after dispatch, before attach ---"
cleanup

tui-use start orchestra
sleep 2
tui-use type "write a hello world script"
sleep 0.3
tui-use press enter
sleep 1

# Wait for ready
if ! wait_for_ready 30; then
    echo "  FAIL: session did not become ready"
    FAIL=$((FAIL + 1))
else
    echo "  PASS: session ready before upgrade"
    PASS=$((PASS + 1))
fi

# Verify tmux session exists before upgrade
SESSION_NAME=$(ls ~/.orchestra/sessions/*.ready 2>/dev/null | head -1 | xargs basename | sed 's/\.ready//')
if tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
    echo "  PASS: tmux session alive before upgrade"
    PASS=$((PASS + 1))
else
    echo "  FAIL: tmux session not found before upgrade"
    FAIL=$((FAIL + 1))
fi

# Upgrade
do_upgrade

# Verify session survived the upgrade
snapshot=$(tui-use snapshot)
assert_contains "session in list after upgrade" "$snapshot" "write-a-hello"

# Verify tmux session still alive
if tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
    echo "  PASS: tmux session survived upgrade"
    PASS=$((PASS + 1))
else
    echo "  FAIL: tmux session died during upgrade"
    FAIL=$((FAIL + 1))
fi

# Attach after upgrade — verify we left the TUI
tui-use press arrow_right
sleep 2
snapshot=$(tui-use snapshot)
assert_not_contains "attachable after upgrade" "$snapshot" "Sessions"

# Detach — tui-use can't trigger tmux key bindings, use detach-client directly
tmux detach-client 2>/dev/null
sleep 2

echo ""

# ---------------------------------------------------------------------------
# Test 2: Upgrade while session is initializing
# ---------------------------------------------------------------------------
echo "--- Test 2: Upgrade while session is initializing ---"
cleanup

tui-use start orchestra
sleep 2
tui-use type "write a comprehensive test suite for the authentication module with unit tests integration tests and mock fixtures"
sleep 0.3
tui-use press enter
sleep 2

# Upgrade while still initializing (don't wait for ready)
echo "  Upgrading while session is initializing..."
do_upgrade

# Verify session survived
snapshot=$(tui-use snapshot)
assert_contains "session survived upgrade during init" "$snapshot" "write-a-comprehensive"

# Wait for ready (might take a while for complex prompt)
echo "  Waiting for session to become ready..."
if wait_for_ready 60; then
    echo "  PASS: session became ready after upgrade"
    PASS=$((PASS + 1))
else
    echo "  FAIL: session did not become ready after upgrade"
    FAIL=$((FAIL + 1))
fi

# Attach after upgrade — verify we left the TUI
tui-use press arrow_right
sleep 2
snapshot=$(tui-use snapshot)
assert_not_contains "attachable after upgrade during init" "$snapshot" "Sessions"

# Detach
tmux detach-client 2>/dev/null
sleep 2

echo ""

# ---------------------------------------------------------------------------
# Test 3: Upgrade after detach, verify re-attach
# ---------------------------------------------------------------------------
echo "--- Test 3: Upgrade after detach, re-attach ---"
cleanup

tui-use start orchestra
sleep 2
tui-use type "write a hello world script"
sleep 0.3
tui-use press enter

# Wait for ready
wait_for_ready 30

# Attach and detach
tui-use press arrow_right
sleep 2
tui-use press arrow_left
sleep 2

# Record session name
SESSION_NAME=$(ls ~/.orchestra/sessions/*.ready 2>/dev/null | head -1 | xargs basename | sed 's/\.ready//')

# Upgrade
do_upgrade

# Verify session still in list
snapshot=$(tui-use snapshot)
assert_contains "session in list after post-detach upgrade" "$snapshot" "write-a-hello"

# Verify tmux session still alive
if tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
    echo "  PASS: tmux session survived post-detach upgrade"
    PASS=$((PASS + 1))
else
    echo "  FAIL: tmux session died during post-detach upgrade"
    FAIL=$((FAIL + 1))
fi

# Re-attach
tui-use press arrow_right
sleep 2
snapshot=$(tui-use snapshot)
assert_not_contains "re-attachable after upgrade" "$snapshot" "Sessions"

# Detach
tmux detach-client 2>/dev/null
sleep 2

echo ""
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
