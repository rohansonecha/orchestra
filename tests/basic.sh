#!/bin/bash
# tests/basic.sh — Basic dispatch, attach, detach test.
#
# Verifies the core TUI flow:
#   1. Dispatch a new session (type prompt + Enter)
#   2. Session appears in list with correct state
#   3. Session transitions from Initializing -> Working
#   4. Attach to session (Right arrow) — see pi
#   5. Detach (Left arrow) — back to TUI
#   6. Session still in list after detach
#
# Run on the main-box: bash ~/orchestra/tests/basic.sh

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
assert_not_contains "session still Initializing" "$snapshot" "◐"

# Attach to session
tui-use press arrow_right
sleep 2
snapshot=$(tui-use snapshot)
assert_contains "attached to pi" "$snapshot" "pi v"

# Detach
tui-use press arrow_left
sleep 2
snapshot=$(tui-use snapshot)
assert_contains "back in TUI after detach" "$snapshot" "Sessions"
assert_contains "session still in list after detach" "$snapshot" "write-a-hello"

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
