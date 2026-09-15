---
name: openorch
description: Use when the user explicitly asks for OpenOrch, asks to consult another installed AI harness, or requests fusion of answers from several harnesses. Configure reusable personal defaults and run supported native consultation channels from Codex or DSH Web.
---

# OpenOrch

Use this skill only for the explicit intent above. Read this installed package's
[HELPER.md](../../HELPER.md) for first setup, configuration changes or errors.
Resolve `../../scripts/openorch.py` from **this SKILL.md's installed directory**.
Never substitute a development checkout or a globally found `orch` for its runtime.

1. Use the user's actual Git project/worktree. It needs an existing commit; do
   not create the project's first commit. Dirty and linked worktrees are supported.
2. On first setup, run the helper's `discover --project ABS` and let the user
   choose supported clients, their native model settings, a single default and
   optionally two to five fusion members. Discovery makes no model calls and
   does not prove login. Read HELPER.md to save those choices with `configure`.
   Reuse `~/.config/openorch/profile.json` on later projects. Use a separate
   `--config-dir ABS` only when the user or acceptance task requests one.
3. Preserve existing project configuration. `attach` creates only a missing
   `.orch/harnesses.yaml`; changing personal defaults does not rewrite projects.
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

DSH Web is a plugin **host**, and DSH, Pi and ZCode are also supported Consult
targets in this release; AGY is not. Actual core discovery and native terminal
evidence are authoritative. This plugin does not add execution/review scheduling
or background orchestration.

Native Codex delegation and waiting own local implementation work. OpenOrch is
only the explicit cross-harness consultation/fusion layer requested by the user.
