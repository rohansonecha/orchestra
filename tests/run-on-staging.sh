#!/bin/bash
# tests/run-on-staging.sh — Run the orchestra test suite on an ephemeral
# staging box, without touching live sessions on main-box.
#
# Usage (run on main-box):
#   bash ~/orchestra/tests/run-on-staging.sh
#
# What it does:
#   1. Launches a test-box (identical to main-box minus OpenClaw)
#   2. Runs tests/run-all.sh on it
#   3. Tears down the box
#   4. Exits with the test's exit code
#
# The box is torn down even if tests fail. If launch fails, exits early.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ORCHESTRA_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER_NAME="test-box-$(date +%s)"

# sky CLI is installed in miniconda3 on main-box but not on default PATH.
export PATH="$HOME/miniconda3/bin:$PATH"

# SKY_INFRA identifies which Kubernetes cluster to launch on.
# Default to the staging cluster; override by setting SKY_INFRA in the env.
export SKY_INFRA="${SKY_INFRA:-k8s/coreweave-dev}"

# Verify sky CLI is available
if ! command -v sky &>/dev/null; then
    echo "ERROR: sky CLI not found. Install with: pip3 install 'skypilot[kubernetes]'"
    exit 1
fi

echo "=== Orchestra Staging Test ==="
echo "Cluster: $CLUSTER_NAME"
echo ""

# --- Launch ---
# cd to orchestra dir so SkyPilot can resolve relative file_mount paths
# (private/models.json, private/agents/work.md)
echo "--- Launching test box ---"
cd "$ORCHESTRA_DIR"
sky launch --infra "$SKY_INFRA" -c "$CLUSTER_NAME" -y "$ORCHESTRA_DIR/skypilot/test-box.yaml"

# --- Run tests, capture exit code ---
TEST_EXIT=0
echo ""
echo "--- Running test suite ---"
sky exec "$CLUSTER_NAME" 'bash ~/orchestra/tests/run-all.sh' || TEST_EXIT=$?

# --- Tear down (always, even on failure) ---
echo ""
echo "--- Tearing down test box ---"
sky down -y "$CLUSTER_NAME"

echo ""
if [ "$TEST_EXIT" -eq 0 ]; then
    echo "=== All tests passed on staging ==="
else
    echo "=== Tests FAILED on staging (exit code $TEST_EXIT) ==="
fi
exit "$TEST_EXIT"
