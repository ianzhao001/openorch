# Balanced agent collaboration

## Main agent and delegation

- Prefer Sol medium for routine projects and Astra medium for complex, ambiguous, or sustained workflows. Respect the user's selected model; do not silently switch it. Increase reasoning only when the task requires it.
- Delegate automatically when a bounded, independent subtask can shorten the critical path, keep substantial exploration output out of the main context, or provide necessary independent verification. This is explicit authorization for conditional delegation, subject to higher-priority instructions.
- Handle simple searches, short answers, single-point edits, and tightly sequential work directly. Usually use 1–2 subagents; use a third only for another useful independent task, subject to runtime limits. Do not fill slots for their own sake.
- The main agent owns requirements, interface decisions, integration, and final acceptance. Continue useful independent work while delegated work runs; do not duplicate the same exploration.
- Subagents must not delegate further by default. Report additional work to the main agent for coordination.
- Each assignment must state the goal, necessary context, scope, file ownership where relevant, and acceptance criteria. Return conclusions, evidence locations, validation, and remaining uncertainty; keep raw logs out of the summary.
- Prefer no-history or limited-history context for independent tasks, with a self-contained task brief. Inherit full history only when prior decisions are necessary. Follow the current tool's model-override and history constraints.
- Assign disjoint file ownership for parallel edits. Serialize changes to the same files or tightly coupled behavior; the main agent checks interfaces even when files differ.

## Role routing and verification

- Use `worker` for bounded implementation, analysis, and drafting; `explorer` for targeted discovery and evidence; `organizer` for extraction, classification, transformation, and structured summaries.
- Use `reviewer` for complex logic, behavior regressions, or meaningful test gaps. Reserve `independent_reviewer` for difficult reasoning, high-impact conclusions, or substantial disputes.
- Model and reasoning settings live in the custom agent configuration files. Prefer named roles over ad hoc overrides, and honor explicit user overrides within runtime constraints.
- Do not require a separate reviewer for every simple, reversible change. Repeat a review only when new evidence, an unresolved risk, or changed critical behavior warrants it. The main agent remains responsible for appropriate project checks.
- Keep project-specific commands, module ownership, and acceptance requirements in the project's AGENTS.md rather than this global file.

## OpenOrch consultation

- Invoke OpenOrch only when the user explicitly requests OpenOrch, Fusion, or consultation through it. A request for native multi-agent work does not authorize external consultation. Follow the installed OpenOrch skill and helper.
- Native subagents divide project work; Fusion supplies independent opinions across clients. OpenOrch is a consultation tool, not an execution scheduler or background orchestrator.
- Ask a specific decision question with relevant facts, constraints, and project text. Preserve existing or explicitly requested members and native model settings. For first-time setup, recommend two complementary members and have the user choose actual clients and models; never invent them.
- Default to one consultation round per decision. Do not automatically add rounds, retry failed members, drop members, or substitute models. Report failures, partial results, and uncertainty honestly.
- Read every member's complete result and the final core summary. Synthesize agreement, useful disagreements, adoption reasons, and verifiable conclusions; majority opinion is not proof.
- Do not automatically send a Fusion-covered decision through another reviewer. Validate actual implementation changes as needed. Continue independent work during consultation, but wait before making changes that depend on its conclusion.
- External clients can consume their own subscription allowances and are not constrained by the native subagent concurrency limit. Preserve existing project configuration; do not initialize or change consultation members merely because this policy is present.

## Economy and calibration

- Optimize total task completion time, observable subscription usage, and rework. Prefer the lowest reasoning effort that meets the quality requirement; avoid redundant context, consultation, and validation.
- For the next 6–10 representative tasks, include a brief calibration note when meaningful: subagent count, Fusion rounds, observed duration/usage if available, and acceptance or rework. Do not invent telemetry or attribute shared account usage precisely to one task.
- If routine exploration or organization misses important evidence repeatedly, improve task boundaries and inputs before raising reasoning for that task class. Tighten review triggers when reviews repeatedly add no useful evidence. Do not promise fixed savings from message counts or API prices.
