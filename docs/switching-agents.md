# Moving a conversation between agents

orchestra can move a running session from one coding agent to another
(Claude Code, Codex, or pi on any model) with `/switch`, fork a Claude Code or
Codex session into pi, and branch a session into a copy. In every case the
new agent should pick up where the old one left off: what was asked, what
was tried, what the tools returned, and what was decided.

The hard part is tool calls. Most of a coding session is the agent calling
tools (running commands, reading and editing files) and reading their
results. Each agent records those calls in its own format, with its own tool
names and rules, and each model provider's API enforces its own constraints
on them. This document lists the problems that creates, why each one matters,
and what orchestra does about it.

The code is in `tui/src/history.rs` (reading and writing each agent's
transcript format) and `tui/src/switch.rs` (deciding what to convert).

## How a switch works

1. Every transcript the session has used is read into one neutral form:
   user and assistant turns made of text, tool calls and tool results.
2. The history is cleaned up so any provider will accept it (see below).
3. It is written out in the target agent's own session format, and the
   target resumes it the way it resumes its own sessions
   (`claude --resume`, `codex resume`, `pi --continue`).
4. A short note tells the model that the conversation was moved and how.

Switching between models within the same agent skips all of this: the agent
already knows how to replay its own history to a different model.

This has been tested end to end with real models: one Claude Code session
that ran a shell command went to pi, back to Claude Code, to Codex, and to pi
on DeepSeek, and each agent answered questions about the earlier turns
correctly.

## The problems

### 1. The agents' tools have different names and arguments

Claude Code calls its tools `Bash`, `Read`, `Write` and `Edit`; pi calls
them `bash`, `read`, `write` and `edit`. The arguments differ too: Claude
Code's `Edit` takes one `old_string` and `new_string`, while pi's `edit`
takes a list of `{oldText, newText}` changes.

**Why it matters.** A model imitates the tool calls in its history. If the
history shows calls to tools it doesn't have, it may try to make them, and
some providers reject a request whose history uses unknown tool names.

**What orchestra does.** It maps the four tools every agent has (run a shell
command, read, write and edit a file) between each agent's names and
arguments. These four account for about 94% of the tool calls in real Claude
Code transcripts. Any other tool call is kept as text describing what was
called and with what arguments.

### 2. A Codex tool call is a small program

Nearly every tool call Codex makes goes through a single `exec` tool whose
argument is JavaScript, for example
`text(await tools.exec_command({cmd: "ls"}))`. One call can run several
commands, loop, or combine results.

**Why it matters.** There is no faithful way to turn an arbitrary program
into a sequence of named tool calls, or the reverse.

**What orchestra does.** A program that makes exactly one command call is
read as that shell command. Anything else is kept as the code itself, as
text. When writing a history for Codex, everything becomes text, because
Codex's tools cannot be replayed from another agent's calls.

### 3. Every call must be paired with its result

Anthropic's API requires every tool call to be followed by its result in the
next turn; OpenAI-compatible APIs have a similar rule. Real transcripts break
this: a turn interrupted before the tool finished, a call whose result was
dropped by compaction, a crash between the two.

**Why it matters.** A single unpaired call makes the provider reject the
entire request, so the switched session would fail on its first message.

**What orchestra does.** A cleanup pass adds a "no result recorded" result
for every unanswered call, turns results whose call is missing into text,
moves results directly after the turn that made the calls, and makes user
and assistant turns strictly alternate, starting with the user.

### 4. Each provider has its own rules for tool-call ids

Anthropic's API accepts only letters, digits, `_` and `-` in a tool-call id;
other providers limit the length or the format.

**Why it matters.** An id that one provider generated can make another
provider reject the request.

**What orchestra does.** It removes characters outside the common safe set,
and marks the imported turns as coming from another model so pi rewrites the
ids into whatever format its current provider needs.

### 5. The model's reasoning can't be carried over

Claude Code stores its thinking signed and with the text left empty; Codex
stores its reasoning encrypted.

**Why it matters.** Replaying signed or encrypted reasoning to a different
provider causes errors. Dropping it loses the "why" behind earlier decisions,
which only survives in what the model wrote out in plain text.

**What orchestra does.** It drops the reasoning.

**Next step.** Before leaving an agent, have it write a short summary of its
decisions and current state for the next agent.

### 6. Compaction summaries differ in whether they can be read

When a conversation gets long, the agent replaces its early part with a
summary. Claude Code's summary is plain text. Codex's is encrypted and only
Codex can read it.

**Why it matters.** After compaction, the summary is the only record of the
early conversation.

**What orchestra does.** It starts the history at the most recent
compaction, which is what the source agent itself would send. For Codex it
notes that the earlier part could not be carried over.

**Next step.** Before switching away from a compacted Codex session, have
Codex write a readable summary and include it.

### 7. Tool output can be huge, and context windows differ

A single command can print megabytes, and the target model's context window
may be anything from 128,000 to a million tokens.

**Why it matters.** A history that doesn't fit makes the new agent's first
request fail.

**What orchestra does.** It keeps the beginning and end of each long tool
output (errors and summaries tend to be at the end), sizes the whole history
to about half of the target model's context window, and when it has to drop
turns, drops the oldest ones, cutting only at a user message so no call
loses its result. The first message is always kept, with a note saying how
many messages were left out.

### 8. The files may no longer match what the history says

The history records what files contained when they were read. After a
switch, a branch or a move to another machine, files may have changed, and a
fork starts from the last commit, without uncommitted edits.

**Why it matters.** A model that trusts an out-of-date read makes edits that
don't apply, or that undo someone else's change.

**What orchestra does.** The handoff note tells the model the files on disk
are the current state and to re-read a file before editing it. `/branch`
copies uncommitted and untracked files into the new worktree.

**Next step.** Include a short `git status` and diff summary in the handoff
note.

### 9. Tools that look the same don't always behave the same

Claude Code's `Edit` has a `replace_all` option that pi's `edit` doesn't.
Claude Code's `Edit` makes one change per call where pi's can make several.
Read offsets and limits, and what counts as an error, differ in small ways.

**Why it matters.** A mapping that changes behavior misleads the new model
about what it actually did.

**What orchestra does.** It maps a call only when the two tools do the same
thing. An edit with `replace_all` is kept as text, and a multi-change edit
becomes one `Edit` call per change when written for Claude Code.

### 10. Each conversion loses a little

Text-only calls, shortened output and dropped reasoning are all small
losses, but a session switched back and forth would lose more each time.

**Why it matters.** A long session that moves between agents would slowly
degrade.

**What orchestra does.** Each session records every transcript it has used.
When it switches back to an agent it used before, orchestra copies that
agent's own original transcript and appends only the turns that happened
since, so that agent's part of the history is never converted. The original
file is never changed, so it stays safe even if it is still open elsewhere.

### 11. Some tools refer to things that only exist in the source agent

Task lists, subagents, background monitors, worktree switches and MCP
servers exist inside the agent that used them. A history full of task
updates means nothing to an agent that has no task list.

**Why it matters.** The new model may try to continue a task list, or wait
for a background job, that isn't there.

**What orchestra does.** These calls are kept as text, so the model can read
what happened but has no call it can repeat.

**Next step.** Have the handoff note list them as not carried over.

### 12. Images

Screenshots and other images can appear in tool results and user messages.
Some models accept images and some don't; a text-only model can reject every
request whose history contains one.

**Why it matters.** One screenshot in the history could make every message
to a text-only model fail.

**What orchestra does.** It replaces images with `[image omitted]`.

**Next step.** Keep images when the target model accepts them, using pi's
model table, which records each model's input types.

## Summary of next steps

- Have the source agent summarize its decisions and current state before a
  switch, which covers dropped reasoning (5) and unreadable Codex compaction
  (6).
- Add a `git status` and diff summary to the handoff note (8).
- List tools that could not be carried over in the handoff note (11).
- Keep images for models that accept them (12).
