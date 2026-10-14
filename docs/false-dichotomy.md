# False dichotomy

There is currently a sort of "false dicthotomy" in how Chat UIs (for example ChatGPT) and Coding agents (for example Codex or Pi) have evolved or been built. Instead of trying to explain it conceptually here is a concrete example of one of my common (daily) workflows:

1. Ask ChatGPT to check the latest Google Search Console results for _all my web properties_ (currently around 5, but growing)

2. Ask for its recommendations on what to work on next

3. Ask it for concrete next actions as a coding agent prompt

4. Copy its prompt. Paste it into the correct project in VS.Code (often - new window; open project; wait for Codex to load; paste the prompt).

5. Either validate/verify the results of the action myself (by manually opening and inspecting the new feature of the website), or copy/paste the results back into ChatGPT for analysis. (Often ChatGPT cannot yet fetch the new website feature itself because ChatGPT caches web content quite heavily).

There are multiple disconnects here:

1. Context

ChatGPT has memory of all my projects, chats, research, opinions and preferences. Occasionally it mixes too much from some other context into the current thread but usually it is EXCELLENT at synthesizing and cross referencing. I often have multiple threads about the same set of web properties, and it remembers what we did last. This enables it to 1) prioritize and 2) generate smart next actions.

But Codex has no awareness of other projects - and often I see when I start a new thread in Codex it hasn't even remembered the basic details of the _current project_'s architecture. Sometimes it even seems to forget to read AGENTS.md.

On one hand, it is good that ChatGPT has full cross referenced and historical context and Codex is focused on one single project (so its actual implementations do not become polluted by other projects). But the workflow is cumbersome and clunky:

- It would be much better if I could tell ChatGPT "yes, this concrete action looks good. Tell the coding agent for that project to implement it." (I don't care how this works, whether it would need a public MCP server, or whatever; but it should be seamless).

- While it's good hygiene for Codex to be unaware of other projects _generally_, it often misses context on the current project -- which could include _information_ found on other projects; or information found while discussing or researching in ChatGPT. So it would be great if I could tell ChatGPT "sync these findings to the coding agent", and it would just do it.

2. Integrations

I built my own MCP server so ChatGPT can access my properties' Search Console data. But Codex cannot access that data.

Codex can access CloudFlare via Wrangler, but ChatGPT cannot (this is less of a problem, but still annoying).

ChatGPT never knows the current state of the websites or the code. It sometimes suggests to do features that are already built, or based on out-of-date knowledge about what's built.

This all suggests we're missing some kind of central knowledge and integrations hub that both can read and write to, so:

- Codex can also read the search console data when necessary
- ChatGPT can look at the site configurations in CloudFlare to figure out which sites have canonical https/http issues (Codex can only do that one site at a time, painfully, with me opening each project separately)
- Codex can tell ChatGPT what it just built; what the website currently looks like (even if just via `fetch`ing the HTML - because ChatGPT always gets heavily cached); what the latest results of an experiment or research were
- ChatGPT can just read the source code to see how our data pipeline works now

## Prior art

I'm aware there's prior art here: some people manage this by moving their entire process to be agent-managed-subagents; or use beads or whatever. But I heavily use the web and IDE user interfaces, and I tell my "planning agent" (ChatGPT) to heavily use the web to do deep, iterative research and synthesize the results of that research. I don't know if a standard agents-in-agents approach fits my needs fully. I also love that I can tell ChatGPT to do some deep research, then go out to do shopping, and check its progress on the ChatGPT app in the shop.

I didn't like OpenClaw's WhatsApp-based way-of-working. I couldn't see what it was doing, and WhatsApp is a super primitive way to communicate.

That being said -- I haven't tried everything that's out there. I know there's AntiGravity and the ChatGPT desktop app (but I don't know if they really help me, and I'm now sufficiently fed up with glitches, weirdness, model outages and juggling and switching between providers that I now just want something in one place that does everything I need.

## OK OK, so what are you really suggesting then, Dave???!

I want:

- A web and mobile app I can use to plan, research, prioritize, and importantly coordinate agentic work. It could be ChatGPT with some MCP orchestration layer (though I have a basic version that already and it's annoying to maintain because ChatGPT Plugins are kind of sucky DX and it's vendor lockin and if a refresh token expires you have to spend 10 minutes hunting in the constantly changing ChatGPT system menu to fix it...). More likely it's my own agentic chat system -- _that has most of the features of ChatGPT_ -- but can also integrate tightly with my work agents.

- A native desktop app I can use to coordinate the actual agentic work (usually but not always coding -- sometimes it's entirely markdown/document backed research). It should be tightly, bidirectionally integrated with the planning app, but have a work focused UX (instead of a planning/freeform focused UX).

- Both systems should be able to do more or less anything the other can do, especially coordinating on information and tasks.
