---
name: openorch
description: Use when the user explicitly asks for OpenOrch, asks to consult another installed AI harness, or requests fusion of answers from several harnesses. Configure reusable personal defaults and run supported native consultation channels from Codex or DSH Web.
---

# OpenOrch

Use this skill only for the explicit intent above. Read this installed package's
[HELPER.md](../../HELPER.md) for first setup, configuration changes or errors.
Resolve `../../scripts/openorch.py` from **this SKILL.md's installed directory**.
Never substitute a development checkout or a globally found `orch` for its runtime.

## Codex host coordination

The packaged [AGENTS.md](../../AGENTS.md) is the published collaboration policy.
When Codex can use native subagents, let its own task/delegation, message, and
wait mechanisms coordinate bounded local implementation work. OpenOrch remains
an explicit cross-harness consultation and fusion layer: use it only when the
user requests OpenOrch, Fusion, or an independent consultation.

Keep one execution owner for each subtask. Do not use OpenOrch to duplicate
Codex's task queue, background scheduling, automatic takeover, or retry
behavior. The current host must synthesize native-subagent and OpenOrch outcomes
with their sources and failures kept distinct; multiple aliases do not by
themselves establish independent model validation.

1. Use the user's actual Git project/worktree. It needs an existing commit; do
   not create the project's first commit. Dirty and linked worktrees are supported.
2. On first setup, run the helper's `discover --project ABS` and let the user
   choose supported clients when not already specified; reuse the user's explicit
   choices without reconfirmation. Save their native model settings, a single default and
   optionally two to five fusion members. Discovery makes no model calls and
   does not prove login. Read HELPER.md to save those choices with `configure`.
   Reuse `~/.config/openorch/profile.json` on later projects. Use a separate
   `--config-dir ABS` only when the user or acceptance task requests one.
3. Preserve existing project configuration. `attach` creates only a missing
   `.orch/harnesses.yaml`; changing personal defaults does not rewrite projects.
   It also registers only this canonical worktree for MCP; restart the server after
   registration changes. An unregistered working directory is never a grant.
4. Save the literal UTF-8 question to a local file. Run
   `python3 ABS_HELPER run --project ABS_PROJECT --mode single|fusion --question-file ABS_FILE`.
   Global `--config-dir`, when needed, goes before `run`. Repeated `--harness ALIAS`
   overrides members only for this invocation. Add only relevant project text
   through repeated `--attach ABS_FILE`; do not include credentials.
5. Wait on the original running handle. Read the final core summary and every
   member's complete answer/artifact. An exit code, partial text, tool-only output
   or unknown native status is not a successful answer. Show unsupported,
   missing-login, failed and partial members honestly; do not silently replace
   models, drop members, retry or add another consultation round.
6. Synthesize in the current host: give the conclusion, useful disagreements,
   limitations/failures and links to original answers. Keep actual model identity
   separate from requested configuration when native evidence is unavailable.

## MCP when available in Codex

After setup/attach, prefer the installed plugin's four tools. Use `list_harnesses`
for allowed-project capabilities, then `consult` with a stable request key and
explicit ordered roles. Use `get_run` for bounded state observation and
`read_answer` to read every verified page, carrying its digest into continuations.
The current host selects and synthesizes; the server never adds a member. For a
request to use saved defaults, obtain them through the Rust-backed helper doctor
and submit those choices explicitly. Do not infer a model identity from an alias.

Same key/input returns its original run; conflicting input is not a retry. Do not
switch backend/model or replay an unclosed run automatically. Explain unavailable
capabilities and unverified/failed members. The recorded Codex ACP Flash/max tuple
is unsupported and must not be replaced with a lower effort or Pro.

If MCP is not available, use the existing helper run path described above with
the same explicit user intent. DSH Web remains a plugin **host** using this
compatibility skill/helper; do not claim DSH MCP support. DSH, Pi and ZCode retain
native Consult channels subject to discovery. AGY Consult/ACP remain unsupported.
No execution/review scheduling or background orchestration is added.
