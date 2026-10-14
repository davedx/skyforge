# Future requirements

Status: planned capabilities, recorded 3 October 2026.

These capabilities follow the [first release](requirements.md). Their order here does not establish an implementation schedule.

## 1. Seamless provider and model switching

Provide one coherent UI for choosing providers, models, and available reasoning levels. It must support models from multiple hosted providers and Qwen running locally on the user's Mac Studio.

- Make switching providers as straightforward as switching models within one provider.
- Show the current provider, model, and reasoning setting clearly in each workstream.
- Preserve conversation continuity when switching, with explicit handling of incompatible context or tool capabilities.
- Expose relevant capabilities, such as image understanding, web search, tool use, and reasoning controls, without pretending every model supports the same features.
- Keep connection/authentication configuration understandable and avoid repeated configuration during normal use.
- Define how the harness reaches Qwen on the Mac Studio, including when the GUI runs on another Mac, and surface connection failures clearly.

**Acceptance:** the user can change from a hosted model to another provider or to Qwen on the Mac Studio from the same workstream UI, see the active selection, and continue with clear feedback about any capability or context constraints.

## 2. Structured, project-focused SQLite memory

Build a thoughtful agent memory system backed by SQLite. It should be structured, useful to coding agents, scoped to the project, and encapsulated behind a clear interface.

This memory serves a different purpose from stored conversation history: it should help agents recover relevant project knowledge without repeatedly rereading every conversation.

Before selecting a design, research existing harnesses and evidence about effective coding-agent memory. The research should assess:

- What deserves persistent memory: project structure, conventions, decisions, constraints, recurring problems, or other durable knowledge.
- How knowledge is extracted and updated, and how conflicting or stale information is handled.
- How retrieval selects relevant information within a limited context budget.
- How memory is grounded in source files, conversations, or other evidence.
- Which approaches measurably improve task quality relative to their latency, token use, and maintenance costs.

Candidate requirements to refine through that research:

- A structured SQLite schema rather than an undifferentiated transcript dump.
- Explicit project boundaries so unrelated projects do not accidentally share memory.
- Provenance and freshness information for retained knowledge.
- User inspection, correction, and deletion of memory.
- Encapsulated read/write/retrieval interfaces, with migration and backup behavior defined.
- A policy for keeping secrets and incidental sensitive command output out of durable memory.

**Acceptance:** a researched design explains the schema, project boundaries, memory lifecycle, and retrieval strategy. Evaluation demonstrates useful knowledge reuse and tests stale/conflicting-memory handling. The final approach remains open until research is complete.

## 3. Project-aware model routing

Status: future capability. The prompt-complexity routing experiment was abandoned on 5 October 2026. Requests currently use manual model selection; a replacement routing approach remains open.

Automatic routing must combine two independent factors: what the prompt asks for, and the complexity and consequences of the object it operates on. That object is typically a repository, but may instead be a document, dataset, or another artifact. Within a repository, assess the targeted area/module when it is known, rather than treating every task as equally complex because it belongs to the same project.

Choose an appropriate provider, model, and reasoning level. Prefer a capable local model when available so routine work consumes no paid cloud tokens. Difficult implementation should go directly to a configured powerful model, for example GPT-6.1-Sol or Opus 5.5 when available. These names express preferences, not a permanent capability ranking.

### Factor 1: task requirements

Any future routing design should account for the requested task, relevant conversation context, and required tools, image support, and context capacity. It should distinguish documentation, targeted code changes, full implementations, debugging, and research without relying on a standalone prompt-complexity score. The approach and evaluation corpus remain open research decisions.

### Factor 2: knowledge of the target object

Progressively build project and module knowledge in SQLite using an initial scan, inexpensive structural heuristics, and observed execution effort. Retain provenance, confidence, scope, and freshness so inferred knowledge can be inspected, corrected, and refreshed incrementally.

**Purpose and consequences:** initially scan project documentation, configuration, and relevant source to identify the primary purpose. Look for real-money handling, live broker/trading integrations, safety-critical behavior, and explicit low-effort prototype intent. Distinguish live execution from simulations or paper trading when evidence permits. Apply a minimum model capability to requests in consequential projects or areas. A simple-looking prompt must not bypass that floor. Explicit prototype intent can reduce the expected engineering effort, but does not erase evidence that a module handles real money or safety-critical operations. Unknown purpose remains unknown rather than being assumed low risk.

**Structural complexity:** measure source lines of code and cyclomatic complexity at repository and module/file scope, excluding generated, vendored, and build output. Consider dependency coupling and language characteristics, including static typing and a borrow checker, as possible weighting features. The direction and magnitude of language weights must be evaluated: these characteristics may increase implementation constraints while also helping detect mistakes. Avoid treating raw LoC or language choice as sufficient evidence of difficulty. Refresh changed areas incrementally rather than rescanning the whole project for every prompt.

**Observed effort:** record how long each prompt takes to perform work on a given repository area, particularly how many output and thinking/reasoning tokens each model consumes. Store this as SQLite **raw processing** data alongside the task requirements identified before execution. Suggested records include:

- Project/object identity, revision or content fingerprint, workstream, prompt/run/attempt identifiers, and timestamps.
- Task requirements, routing policy version, and decision rationale at routing time.
- Initially estimated target areas and subsequently observed areas from file reads, edits, and tools; retain both because the true scope may emerge during execution.
- Provider, exact model/version, reasoning setting, route rationale, and any escalation between models.
- Wall-clock duration, time to first substantive action, model generation time and tool/wait time where available, input tokens, output tokens, and reasoning tokens.
- Completion, cancellation, failures, retries, validation results, and available user/review feedback about quality.

Record each model attempt separately and aggregate at run and area level without double-counting a run that touches multiple areas. Providers expose different usage fields: preserve raw usage and distinguish output totals from reasoning subsets. Missing reasoning-token counts must remain unavailable, not zero or fabricated. Retain partial usage for failed and cancelled attempts where reported.

Raw processing measures effort, not correctness or intrinsic difficulty. Slow hardware, tool waits, verbose models, reasoning settings, and unsuccessful attempts can inflate measurements. Derive comparable per-task/per-area estimates using model identity, settings, outcome, and enough observations; do not automatically convert high token counts into difficulty labels. A single long run should not permanently mark an area as difficult.

### Combining the factors

1. Identify task requirements and the likely target object/areas.
2. Retrieve target purpose, risk floor, structural measurements, and relevant processing history.
3. Determine required model capability from both factors, preserving the applicable project/area minimum. Check tools, image support, context capacity, and provider availability.
4. Select the least costly qualified model, preferring local execution. Keep the routing decision local as well so a routine local task does not incur paid routing or embedding calls.
5. Show the selected provider/model/reasoning setting and a concise explanation. Support automatic routing being off, manual overrides, and a fixed selection; make departures from an inferred capability floor visible.
6. Record actual execution and outcomes to refine future estimates. Escalate when validation failures or lack of progress show the selected model is inadequate. Define unavailable-provider fallback behavior without silently dropping below the automatic capability floor.

Examples: summarizing recent work into a Markdown file should normally use a capable local model with sufficient context. A targeted change in live trade execution may require a powerful model despite a short prompt. A full implementation specification spanning complex modules should route to the configured powerful model. An explicitly disposable prototype can use a cheaper route when its scope and consequences support that choice.

**Acceptance:** evaluate the same prompt against targets with different complexity and consequences and verify that routing changes appropriately. Demonstrate local routing and execution for routine documentation without paid cloud calls, direct powerful-model selection for complex work, enforcement of automatic capability floors, inspectable SQLite raw processing records including unavailable usage fields, incremental knowledge updates, and understandable manual overrides. Evaluate task completion/quality alongside cloud token cost, latency, routing overhead, and escalation rate; numerical thresholds and the routing approach remain open decisions.

## 4. Automatic independent review

When an agent undertakes complex work, automatically run a separate, isolated review of the resulting changes. Assess maintainability, correctness, and security.

- Define what counts as complex work and make the trigger understandable to the user.
- Review the actual changes and enough relevant project context to assess them.
- Use a separate review context that supports an independent assessment rather than inheriting the implementation agent's conclusions as fact.
- Define isolation for reviewer state and execution. The review must not silently modify the implementation being assessed.
- Present findings with severity, affected locations, supporting evidence, and an explanation of why they matter.
- Offer menu actions to fix a finding automatically, create an issue, or save a note.
- Define where issues and notes are stored or sent; external issue-tracker integration is an open decision.
- Track the user's disposition of findings so they can be revisited without repeatedly raising already handled items.
- Recheck relevant findings after an automatic fix and report the result.

This isolation requirement does not change the first-release preference for simple workstream tabs without mandatory git worktrees. The review mechanism must be designed separately.

**Acceptance:** a complex change triggers an independent review, findings cover the specified quality dimensions, and the user can choose an automatic fix, issue, or note through the GUI. Fixes and recorded decisions remain traceable to their findings.

## Research and design outputs

Before implementation of these features, document:

1. Provider capability and connection abstractions, including Qwen access on the Mac Studio.
2. Evidence and tradeoffs for the memory design, with an evaluation plan.
3. Routing criteria, manual overrides, and failure behavior.
4. Review triggers, isolation guarantees, finding format, and issue/note destinations.

These remain future implementation and research tasks. Provider comparison, detailed memory design, replacement routing research, and routing implementation remain outstanding.
