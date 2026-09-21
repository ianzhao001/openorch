# OpenOrch setup and consultation

The installed Python helper verifies the packaged runtime and forwards literal
arguments to Rust. Rust owns profile validation, defaults, project setup and
consultation. Validation uses the same harness parser as CLI/MCP/Web; it creates
no disposable Git clone. No daemon or background scheduler is added.

Install the complete local release bundle. Source files alone have no runtime.
Codex can use the packaged MCP server; DSH Web retains its existing skill/helper
entry. This release does not claim DSH MCP integration.

## Existing helper commands

Run `python3 /absolute/plugin/scripts/openorch.py --help`.
Global `--config-dir ABS` goes before the action. Otherwise Rust uses
`OPENORCH_CONFIG_DIR`, when explicitly set for this process, or
`~/.config/openorch`. This does not change native client defaults.

| Action | Behavior |
| --- | --- |
| `discover --project PATH` | List installed executable candidates and core consultation capabilities; no model call or login claim. |
| `configure --project PATH --input PROFILE_JSON` | Validate explicit choices and create a private personal profile. |
| `configure ... --replace-profile` | Back up and explicitly replace only the personal profile. Existing project configuration stays unchanged. |
| `attach --project PATH` | Initialize only missing project configuration and register this exact worktree for MCP. |
| `doctor --project PATH` | Inspect configuration and optional saved defaults without inference. |
| `run --project PATH --mode single --question-file FILE` | Consult one saved or explicitly selected member. |
| `run --project PATH --mode fusion --question-file FILE` | Consult two to five distinct explicit members. The host synthesizes their answers. |

Repeated `--harness ALIAS` overrides members for this invocation only. Repeated
`--attach PROJECT_TEXT_FILE` supplies project-local text; relative attachments
are resolved against the target worktree. Questions are UTF-8 literal files and
are never passed to a shell. No member, provider, model or effort is silently
substituted. Native consultation deadlines remain owned by the shared core.

## Personal profile and recovery

A profile contains exactly `version`, `harnesses` and `defaults`. The harness map
uses the existing core schema, including supported `defaults`, action overrides
and the opt-in `consult.acp` block. Executable paths must be absolute. Credentials
belong in their existing secure sources; `credentialEnv` is a variable name.

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
  "defaults": {"single": "advisor", "fusion": []}
}
```

Choose actual installed clients and model settings before saving. The example
does not certify a provider/model combination. The single default must exist;
fusion is empty or contains two to five distinct supported aliases. An empty
fusion cannot run. Unknown fields, unsafe paths and unsupported defaults fail.

Files are private (0600), with short setup locks and complete atomic publication.
Explicit replacement first saves and verifies the previous snapshot. It then
uses atomic exchange to retain the actual overwritten file as well, closing the
last-check/replacement race. The JSON result identifies `backup` and
`snapshotBackup`. Existing files are never overwritten during first creation.
If atomic exchange is unavailable, replacement refuses without a rename fallback.

To restore a reviewed backup through the same validation, run:

```sh
python3 /absolute/plugin/scripts/openorch.py --config-dir /absolute/personal-dir configure --project /absolute/project --input /absolute/backup.json --replace-profile
```

The current profile is backed up again; old backups remain. If a concurrent edit
or directory-sync error occurs after exchange, the error identifies the retained
actual old file and the earlier snapshot. Inspect these paths before retrying:
an error does not claim that publication was rolled back. No automatic rollback
can overwrite a concurrent editor's changes.

## Project configuration and authorization

The target must be an existing non-bare Git worktree with a commit. Dirty and
linked worktrees are supported. `.orch/harnesses.yaml` belongs to the core's
shared primary-repository configuration location. Existing bytes are validated
and retained exactly; changing the personal profile does not rewrite projects.

Only generated paths are appended to local Git `info/exclude`: `.orch/`,
`coordination/consultations/`, and `.cowork-temp/channel-capture/`. Tracked
`.gitignore`, Git configuration and project source are untouched. If later
initial publication fails, these generated ignore entries may already exist;
the error says so and an existing target is not overwritten.

`attach` also registers the canonical requested worktree and its local directory
identity in private `projects.json`, at most32 projects. Registration does not
implicitly authorize sibling worktrees or the current directory. A replaced or
removed directory fails registry validation. Explicitly reattach the intended
worktree; review stale registrations when moving projects. A running MCP process
keeps its startup allowlist, so restart it after changing registration.

## Codex MCP

The package declares exactly four tools: `list_harnesses`, `consult`, `get_run`,
`read_answer`. The launcher verifies all runtime resources before replacing itself
with `orch-mcp`; stdout is reserved for protocol frames. Setup first, then restart
the host/server to load the registered projects. An empty registration fails
closed. It never infers access from the host's working directory.

For an explicit host configuration, launch either:

```sh
/absolute/plugin/runtime/orch-mcp --project /absolute/worktree
/absolute/plugin/runtime/orch-mcp --projects-file /absolute/personal-dir/projects.json
```

These selectors cannot be mixed. Without arguments, the packaged plugin reads
the shared personal registry selected above. No tool accepts commands or arbitrary
environment values. Each call names an allowed project. `consult` takes a request
key, question, explicit ordered role members and optional attachments. Same key
and input returns the original run; changed input conflicts. No automatic retry,
model fallback or synthesis member is added. The host plans, chooses and combines.

`get_run` returns metadata and diagnostic state, with at most30 seconds of bounded
waiting. Use `read_answer` for verified UTF-8 pages, retaining the full-answer
SHA-256 for continuation. Read every complete member answer before synthesizing.
Tool-only output, missing trusted termination, wrong identity or changed answer
bytes are not successful answers. Shutdown stops admission and drains bounded
work; host exit does not promise continued background execution.

## Runtime and qualification

The complete inventory contains `orch`, `orch-mcp`, `orch-acp` and four wrappers
under `runtime/scripts/`: `wake-multica.sh`, `wake-dsh-stream.sh`,
`wake-pi-stream.sh`, `wake-zcode-stream.sh`. Every entry has a byte count and
SHA-256 and must be an ordinary executable. All three binaries report version
0.1.0; `orch guide --check` verifies the six public leaves. Missing, modified,
symlinked or non-executable resources refuse before invocation. Checksums detect
changes; they do not attest publisher identity.

Build the default CLI separately from protocol binaries. It has no UI, protocol
SDK or Tokio dependency. Its private helper entry retains normal project-mode,
stale-binary and consultation admission checks. Setup errors return2; once an
actual consultation starts its existing CLI outcome/partial-success semantics
remain authoritative.

ACP is explicitly selected per Consult profile. Existing native backends remain
available by explicit configuration; a failed ACP request never switches backend.
OpenCode and Claude have recorded real DeepSeek Flash/max qualifications. Codex
ACP1.12.0 with native0.155.1 cannot negotiate the requested effort=max and refuses
before Prompt; it is not certified for that tuple. AGY keeps its existing high
model selection and does not support Consult or ACP. DSH, Pi and ZCode retain
native compatibility consultation channels subject to discovery. Discovery,
requested settings and actual model evidence are distinct.
