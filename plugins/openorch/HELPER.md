# OpenOrch helper

The Python 3 standard-library helper configures personal defaults and delegates
to the installed `runtime/orch`. It adds no core commands and starts no daemon.
Install the complete release bundle; the source checkout alone has no runtime.

The shared skill resolves this helper from its installed package. Codex uses
its native plugin installation; DSH Web uses a separate filesystem-skill provider
with package-relative resources. See [README.md](README.md) for native install,
update and unregister operations. The setup inspection commands have no helper
timeout; actual consultation deadlines remain owned by the core/native channel.

## Commands

Run `python3 /absolute/plugin/scripts/openorch.py --help`.
Each command also accepts `--help`. Global `--config-dir ABSOLUTE_DIRECTORY`
selects a separate personal profile; the default is `~/.config/openorch`.

| Command | Purpose |
| --- | --- |
| `discover --project PATH` | Find installed native clients and ask the core which support consultation; no model calls. |
| `configure --project PATH --input PROFILE_JSON` | Validate and save chosen defaults. Add `--replace-profile` to explicitly update an existing profile. |
| `attach --project PATH` | Create only a missing project configuration from the personal profile. |
| `doctor --project PATH` | Verify the bundled runtime and inspect project configuration through the core. |
| `run --project PATH --mode single --question-file FILE` | Consult the saved single member. |
| `run --project PATH --mode fusion --question-file FILE` | Consult the saved fusion group. |

Repeat `--harness ALIAS` on `run` to select members for that invocation.
Aliases follow the core grammar: an ASCII letter or digit first, then ASCII
letters, digits, dashes, underscores or dots (for example `advisor.primary`).
Single requires one member; fusion requires two to five distinct members.
Repeat `--attach PROJECT_TEXT_FILE` for text context. Relative attachments are
resolved against the target worktree; the core validates paths and size limits.
Do not interpret raw process output or an exit code alone as a valid answer:
read the core's final summary, per-member outcomes, and artifact locations.

## Configure once, reuse across projects

Discovery finds executable paths. It does not prove login, model availability,
credentials, or successful answers. Select only native clients already installed
and logged in, and choose model settings supported by those clients.
The helper never substitutes a different provider or model.

An example personal profile follows. Replace the executable and model settings
with the actual local choices before saving; do not copy credentials.

```json
{
  "version": 1,
  "harnesses": {
    "advisor": {
      "driver": "codex",
      "executable": "/absolute/path/to/codex",
      "enabled": true,
      "cwdPolicy": "project-root"
    }
  },
  "defaults": {
    "single": "advisor",
    "fusion": []
  }
}
```

The `harnesses` object uses the core's version 1 schema, including supported
`defaults` and action-specific `consult` settings. Unknown fields, raw argv/env,
invalid paths, unsupported defaults, and malformed profiles are rejected by
the helper or core. The single default must exist. Fusion may remain empty
until two to five supported aliases have been chosen. Empty fusion cannot run.

Validation uses a temporary local shared clone with no checkout. It invokes
only local core configuration/discovery commands and removes that clone when
finished. It does not run providers or change the user's Git configuration.
The profile is published atomically with mode 0600. Existing profiles require
explicit `--replace-profile`; replacement does not update existing projects.

## Project state and invocation

Projects require Git and an existing commit. An uncommitted or dirty worktree is
allowed. OpenOrch never creates the project's first commit.
The core configuration contains only `version` and `harnesses`, stored at
`.orch/harnesses.yaml` in the main repository. JSON is valid YAML input here.
Linked worktrees share that configuration through the Git common directory.

Existing project configuration is validated and its bytes are preserved, even
when personal defaults change. To revise it, explicitly edit that project file
and validate it with `doctor`; OpenOrch does not merge or overwrite it silently.
A short local lock and atomic no-clobber publication serialize initial setup.
Symlink configuration files and symlink directory components are rejected.

Only generated paths are appended to Git's local `info/exclude`:
`.orch/`, `coordination/consultations/`, and `.cowork-temp/channel-capture/`.
Tracked `.gitignore`, Git configuration, and project source files are untouched.

Single and fusion both invoke the existing core `consult` command with explicit
members. The helper replaces its process with the core, preserving signals,
stdout/stderr and exit status. The working directory and `--root` are the user's
actual target worktree, including when it is dirty or linked. UTF-8 question
bytes are staged to a unique local ignored request file and passed by literal
argv. Question text is never interpreted by a shell. Explicit members override
only this invocation; no member is dropped or substituted. Unsupported explicit
members reach the core's actual validation and per-member failure handling.

## Runtime and capabilities

Before every operation the helper verifies `runtime/manifest.json` version 1.
It must list exactly `orch` and the four scripts
`wake-multica.sh`, `wake-dsh-stream.sh`, `wake-pi-stream.sh`,
`wake-zcode-stream.sh` beneath `runtime/scripts/`.
Each entry contains `sha256` and `bytes`. Every resource must be an ordinary
file, and `orch guide --check` must report the default six-command surface.
Missing, changed or symlinked resources fail before an invocation. Reinstall
the complete version; a development checkout is not an integrity fallback.
Checksums detect local changes, not publisher identity or reproducible builds.

The current core supports consultation for Codex, Claude, OpenCode, Cursor,
MiMo, CodeBuddy, SmartClaw, DSH, Pi and ZCode, subject to actual configuration
discovery. AGY is not a consultation target in this core release. DSH Web can
host this plugin and DSH can also be a target; host integration and target
capabilities are separate. Runtime discovery is the final authority.

The source checkout also contains an independent read-only `orch-tui` binary for
captured invocation observations. It is not part of the default six-command
prebuilt runtime and never starts, cancels, collects, reconciles or garbage-
collects an invocation.

It also contains a source-built loopback-only `orch-web` frontend. It serves
read-only observation data only from `127.0.0.1`, never opens arbitrary project
files, and is not included in the prebuilt runtime.

## Errors and recovery

Helper setup/validation errors print an actionable message to stderr and return
2. CLI syntax errors also return 2. Once the core starts, its own documented
exit status and evidence are preserved. A failed member is not a successful
fusion result. Read the complete core artifacts before drawing conclusions.

For missing login, use the native client's own login flow. For invalid
configuration, correct the selected profile or existing project file explicitly.
For a symlink conflict, choose an ordinary owned directory. If a first project
validation fails, the helper preserves its newly created configuration for
diagnosis; no provider has run at that point. Configuration files may contain
local paths and model selections: keep them local and out of published bundles.
