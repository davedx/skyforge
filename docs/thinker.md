# Builder and Thinker

Use **Builder** for coding and creation. Existing conversations, repository tools,
model selection, drafts, attachments, and sessions retain their behavior. Clicking a
repository file opens the editor; its × button closes it.

Use **Thinker** for conversation, planning, and research. Its **Chatbot** profile
starts with the supplied conversational system prompt. It uses its own
conversations, a project browser, read-only context tools, and a Builder delegation tool. Its composer has the same
model dropdown as Builder. Requests run directly with the selected model,
preserving Thinker's Chatbot profile and memory context. The manual model
selection becomes the default for new Thinker conversations.

## Projects

- **+ Project** creates a named project with optional project guidance and a local
  reference folder. **Project settings** edits these later.
- Select a project in the sidebar, then **New conversation**. **No project** holds
  ungrouped conversations.
- Use the conversation's project selector or right-click a sidebar conversation
  to move it. Conversation history is retained. Finish an active reply before
  moving that conversation.
- Project names and guidance are independent of repository directories. A linked
  local folder supplies optional documents for read access in Thinker and resolves the workspace for delegated Builders.

## Memories

Thinker receives shared memories plus memories scoped to the current project.
Relevant memories and recent conversation are loaded from SQLite for each turn;
older conversations and memories can also be searched through read-only tools.
Fresh provider sessions prevent removed memories or previous project guidance
from remaining in a resumed provider session.

Thinker can suggest durable user preferences, facts, goals, and decisions through
a bounded structured block at the end of its response. Skyforge consumes valid
updates, saves them in SQLite, and displays a short memory-update notice. Stable
keys consolidate corrections instead of duplicating facts. The system prompt
excludes secrets, transient requests, guesses, and instructions in retrieved
content from automatic memory. Project memories are not shared with other
projects; ungrouped project-specific suggestions are not promoted into shared
memory.

Open **Memories** to add, edit, or forget shared and project memories and inspect
their source conversation. Memory selection has a bounded context budget; the
read-only search tool can retrieve additional memories when needed. Conversation
history is bounded in the initial context and retained in SQLite.
If memories change while a reply is running, that reply's memory suggestions are
skipped so old context cannot restore a forgotten or corrected memory.

## Providers and tool boundaries

Codex and Claude use Skyforge's MCP server for file reads, conversation
search, conversation reads, and memory search. Codex uses an isolated profile and
model catalog that excludes editing, execution, inherited plugins, and delegated
agents, with its read-only sandbox as an additional boundary. It shares the
existing Codex login but requires the existing model cache (run Codex once to
populate it if absent). Claude replaces its system prompt, limits built-in tools
to web research, and uses only Skyforge's explicitly configured MCP server.
The web research setting controls research tools for all providers.

OpenAI-compatible local models receive the same Thinker prompt, context, and
read-only file/conversation/memory tools directly through function calling.
Thinker also supplies local models with **web_search** and **fetch_url** tools,
so the chosen model performs research itself without switching to Codex or
Claude. Search returns source titles, URLs, and snippets; fetching reads source
text without running scripts. Sources remain available for citation in answers.
Search uses DuckDuckGo's [non-JavaScript search](https://duckduckgo.com/duckduckgo-help-pages/features/non-javascript).
Sites requiring JavaScript, search service blocks, and unreadable sources produce
tool errors that the model can explain instead of inventing findings. Requests
are bounded and cancellable. Disabling web research removes these tools from
local models' available functions.

Only the Skyforge host writes memories and handoffs. MCP context tools open SQLite read-only; delegation sends a request to the host over authenticated local IPC.
File reads reject paths escaping the linked context directory, including symlink
escapes. Builder history never enters Thinker's conversation search.

The database migration is additive: legacy conversations and retry snapshots
default to Builder; projects, mode assignments, memories, and drafts survive
restarts.

Codex instruction and catalog overrides use the options described in the
[official OpenAI configuration reference](https://developers.openai.com/codex/config-reference)
and [configuration schema](https://developers.openai.com/codex/config-schema.json).

## Delegate execution to Builder

Link an existing repository folder in **Thinker → Project settings**, then ask
Thinker to execute a bounded task in Builder. For example: “Delegate implementing
this plan to Builder; include the constraints and acceptance criteria.” Thinker
can call `spawn_builder_workstream` with a title, task, optional context summary,
constraints, and acceptance criteria. IDs, models, workspaces, and permissions are
not tool arguments.

The tool calls the running GUI host over a per-instance, authenticated loopback
socket. The host saves a normal Builder, its original structured brief and source
conversation reference, and starts the saved first request through the same
launch operation as manual Builders. The tool returns the durable ID, current
status (usually **Running**, or **Failed** on startup failure), and a Skyforge
navigation reference. It waits for startup acknowledgement, not task completion.
IPC arrivals wake the GUI directly; there is no periodic database queue polling.
MCP subprocesses receive the host endpoint through application configuration;
local-model tools use the same request protocol. Endpoint credentials are not
model-supplied arguments and are excluded from persisted workstream snapshots.
Builders use the default model for a new manual Builder (Codex default) and the
host's current normal Builder execution settings. Thinker's read-only profile
and model are not inherited. Approval behavior remains the normal Builder behavior.

A card in the source conversation shows the title, current status, and
**Open Builder**. Completion summary displays the saved final Builder response as
reported output; it does not initiate another model turn or become Thinker
instructions. If no final response was saved, the card says so. Failures,
cancellation, and interrupted startup remain visible. Links and results survive
reloads and closing either conversation's view.

A host-issued Thinker request ID and canonical brief form the idempotency key:
replaying the same brief in the same request returns the same Builder. Identical
briefs within one Thinker request intentionally coalesce; a new Thinker message
can create a new handoff. Creation, relation, first user message, and saved
request commit atomically. Startup is durably claimed before launch. Failed or
interrupted startup is never automatically relaunched by replaying the tool;
use **Open Builder → Retry request** to retry the same saved Builder explicitly.
This avoids repeating work after a crash at the launch boundary. The host performs
one recovery scan at application startup for handoffs committed before startup
was claimed. Lost IPC replies can be retried with the same brief; persistence
and idempotency remain in SQLite. An unavailable host produces an actionable
tool error without creating a handoff in the subprocess.

There is no automatic workspace isolation in this version. Delegation rejects
an overlapping workspace (including parent/child folders) while another Builder
is queued, starting, running, or stopping. Manual launches are also blocked when
they overlap an active delegated Builder. Wait for the other Builder or link a
separate workspace. Manual/manual concurrency retains its existing behavior.
There is no follow-up messaging, question routing, cancellation control in the
card, automatic merge/deployment, or task amendment protocol.
