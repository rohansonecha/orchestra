#!/bin/bash
# tests/basic.sh — Basic dispatch, attach, detach test.
#
# Verifies the core TUI flow:
#   1. Dispatch a new session (type prompt + Enter)
#   2. Session appears in list with correct state
#   3. Session transitions from Initializing -> Working
#   4. Attach to session (Right arrow) — see pi
#   5. Detach (tmux detach-client) — back to TUI
#   6. Session still in list after detach
#   7. Left arrow keybinding is set for real users
#
# Note: tui-use can't trigger tmux root-table key bindings (Left arrow,
# Ctrl+C) because tmux attach reads from /dev/tty, not the PTY that
# tui-use writes to. We use `tmux detach-client` directly to test the
# detach behavior, and separately verify the keybinding is set.
#
# Run on the test box (via tests/run-on-staging.sh):
#   bash ~/orchestra/tests/basic.sh
# Do NOT run directly on main-box — it kills all tmux sessions.

set -euo pipefail

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
        echo "  --- snapshot debug ---"
        echo "$haystack" | head -15
        echo "  ---"
        FAIL=$((FAIL + 1))
    fi
}

assert_not_contains() {
    local desc="$1"
    local haystack="$2"
    local needle="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  FAIL: $desc (did not expect '$needle' in output)"
        echo "  --- snapshot debug ---"
        echo "$haystack" | head -15
        echo "  ---"
        FAIL=$((FAIL + 1))
    else
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    fi
}

cleanup() {
    tui-use kill 2>/dev/null || true
    pkill -x orchestra 2>/dev/null || true
    tmux kill-server 2>/dev/null || true
    sleep 1
    rm -rf ~/.orchestra/sessions/* ~/orchestra/worktrees/* ~/work-repos/prototype/.orchestra/worktrees/* 2>/dev/null || true
    git -C ~/work-repos/prototype worktree prune 2>/dev/null || true
}

trap cleanup EXIT

echo "=== Basic Dispatch/Attach/Detach Test ==="

# Clean state
cleanup

# Start TUI
tui-use start orchestra
sleep 2

# Verify TUI is running
snapshot=$(tui-use snapshot)
assert_contains "TUI started" "$snapshot" "Sessions"

# Dispatch a session
tui-use type "write a hello world script"
sleep 0.3
tui-use press enter
sleep 1

# Verify session appears in list
snapshot=$(tui-use snapshot)
assert_contains "session in list" "$snapshot" "write-a-hello"
assert_contains "dispatched status" "$snapshot" "Dispatched"

# Wait for session to be ready (up to 30 seconds)
echo "  Waiting for session to be ready..."
READY=0
for i in $(seq 1 60); do
    if ls ~/.orchestra/sessions/*.ready 2>/dev/null | grep -q .; then
        READY=1
        break
    fi
    sleep 0.5
done
if [ "$READY" -eq 1 ]; then
    echo "  PASS: session became ready"
    PASS=$((PASS + 1))
else
    echo "  FAIL: session did not become ready within 30s"
    FAIL=$((FAIL + 1))
fi

# Verify state transitioned to Working (●)
sleep 1
snapshot=$(tui-use snapshot)
assert_contains "session shows Working state" "$snapshot" "●"
assert_not_contains "session still shows Initializing icon" "$snapshot" "◐"

# Attach to session — verify we left the TUI (no "Sessions" title)
tui-use press arrow_right
sleep 4
snapshot=$(tui-use snapshot)
assert_not_contains "left TUI after attach" "$snapshot" "Sessions"

# Detach — tui-use can't trigger tmux key bindings (see header comment),
# so we use tmux detach-client directly. This tests the same behavior:
# detaching brings us back to the TUI without killing the session.
tmux detach-client 2>/dev/null
sleep 2
snapshot=$(tui-use snapshot)
assert_contains "back in TUI after detach" "$snapshot" "Sessions"
assert_contains "session still in list after detach" "$snapshot" "write-a-hello"

# Verify the Left arrow keybinding is set for real users
# (tui-use can't test this interactively, but we can verify it's configured)
keybindings=$(tmux list-keys -T root 2>/dev/null)
assert_contains "Left arrow bound to detach" "$keybindings" "Left"
assert_contains "Left arrow detaches" "$keybindings" "detach-client"

# Verify tmux session still alive after detach
if tmux has-session -t "$(ls ~/.orchestra/sessions/*.ready | head -1 | xargs basename | sed 's/\.ready//')" 2>/dev/null; then
    echo "  PASS: tmux session alive after detach"
    PASS=$((PASS + 1))
else
    echo "  FAIL: tmux session died after detach"
    FAIL=$((FAIL + 1))
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
