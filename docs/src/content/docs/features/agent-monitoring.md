---
title: Agent Monitoring
description: Monitor AI coding agents running across your tmux sessions.
---

pertmux detects and monitors AI coding agent instances running in tmux panes across all your sessions.

## Supported agents

pertmux supports two coding agents:

- **[opencode](https://github.com/sst/opencode)** (v2) — requires no special flags. Status is read from the opencode background service's HTTP API.
- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — requires no special flags. Status is detected by reading JSONL transcript files from `~/.claude/`.

See [Agent Configuration](/configuration/agent-config/) for setup details.

The architecture is pluggable — new agents can be added by implementing the `CodingAgent` trait. See [Extending pertmux](/reference/extending/) and [Contributing](/reference/contributing/).

## How detection works

Every 2 seconds (configurable via `refresh_interval`), the daemon:

1. Lists all tmux panes across all sessions
2. Checks each pane's running process against registered agent process names (`opencode`, `claude`)
3. For matched panes, queries the agent for status using its own mechanism:
   - **opencode**: Matches the pane's terminal title to a session and queries the background service's API
   - **Claude Code**: Reads JSONL transcript files from `~/.claude/` and infers status from the last entry
4. Enriches each pane with session details (title, model, tokens, messages)
5. Links each agent pane to its corresponding MR via the worktree path

## Agent status

Each detected agent shows a status badge:

| Status | Meaning |
|--------|---------|
| **Busy** | Agent is actively working (generating code, running tools) |
| **Idle** | Agent has finished its current task |
| **Retry** | Agent encountered an error and is retrying |
| **Unknown** | Status could not be determined |

Status priority for display: Busy > Retry > Idle > Unknown.

## Agent Actions

Press **`a`** on a worktree with an active agent session to send commands to the agent — rebase, fix pipeline failures, and more. Actions are delivered via HTTP API for opencode and via tmux send-keys for Claude Code. See [Agent Actions](/features/agent-actions/) for details.

## Global session search

Press **`S`** anywhere in the dashboard to fuzzy-search every agent session pertmux can see, across all tmux sessions and projects. Each result shows the session title, status badge, agent name, `session:window.pane` location, and time since last activity. Queries match the session title, the tmux session name, and the worktree directory, so `mainapi`, `sess-name`, or a few words of the title all work.

Press `Enter` to jump: pertmux switches tmux focus to that pane and points the dashboard at the card that owns it — the MR if the pane is linked to one, otherwise the worktree at its path. The key is configurable via `session_search` in `[keybindings]`.

One row is shown per pane running an agent, since that is what `Enter` can switch to.

Press `Delete` to kill the highlighted agent. This runs `tmux kill-pane` on that pane only — the plain shell pane that pertmux opens beside an agent survives, and tmux closes the window if the agent was its last pane. There is no confirmation, and any work the agent had in flight is lost.

## Session details

When you select an agent pane, the detail panel shows:

- **Working directory**
- **Token usage** (input and output tokens)
- **Message count** and session duration
- **File changes** (files modified, additions, deletions)
- **Todo list** with completion status
- **Message timeline** with role indicators and text previews

## Agent-only mode

If you don't configure any forge (`[gitlab]` or `[github]`), pertmux runs in agent-only mode — showing a simple list of all detected coding agents grouped by tmux session.
