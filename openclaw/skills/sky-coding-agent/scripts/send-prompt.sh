#!/bin/bash
# send-prompt.sh — Send a prompt to pi on a work-agent sandbox.
#
# Usage: send-prompt.sh <task-id> "your message here"
#
# Uses pi -p (print mode) for one-shot prompts. The pi session persists
# inside the sandbox, so repeated calls to the same task-id maintain context
# (pi loads the session by cluster name).

set -euo pipefail

TASK_ID="$1"
PROMPT="$2"
STATE_DIR="/tmp/orchestra-sandboxes"
CLUSTER_FILE="$STATE_DIR/${TASK_ID}.cluster"

if [ ! -f "$CLUSTER_FILE" ]; then
    echo "Error: No sandbox for task '$TASK_ID'. Run launch-sandbox.sh first." >&2
    exit 1
fi

CLUSTER_NAME=$(cat "$CLUSTER_FILE")
SESSION_NAME="work-agent-${TASK_ID}"

# Send the prompt via pi -p (print mode) over sky ssh.
# --name sets the session name for multi-turn continuity.
sky ssh "$CLUSTER_NAME" \
    "pi -p $(printf '%q' "$PROMPT") --provider glm --model zai-org/GLM-5.2-FP8 --name '$SESSION_NAME'" 2>&1
