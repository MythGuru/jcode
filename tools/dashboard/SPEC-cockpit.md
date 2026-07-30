# Dashboard Upgrade Spec: Interactive Cockpit (3 Phases)

**Author:** Atlas (managing session jaguar). **Implementer:** Eve.
**Scope:** ONLY `tools/dashboard/server.js` and `tools/dashboard/dashboard.html` (plus `tools/dashboard/README.md` doc updates). Do NOT touch any Rust code, any crate, or any file outside `tools/dashboard/`.

## Context you need first

Read these before writing code:
- `tools/dashboard/server.js` (~266 lines) - the entire backend. Note the existing helpers: `sendDebug(pipe, command, sessionId)`, `pipeNameFromSocket`, `loadLiveSessions`, `rejectBadOrigin`, `readBody`, and the existing POST endpoint `/api/session/new`.
- `tools/dashboard/dashboard.html` - the entire frontend.
- The debug socket protocol: newline-delimited JSON `{type:"debug_command", id, command, session_id?}` over a named pipe. Existing commands used: `sessions`, `tool:memory {...}`.

Debug commands you will use (already implemented in jcode, do not modify them):
- `queue_interrupt:<text>` - queues text into the target session as user input at the next safe point. Returns `"queued"`.
- `queue_interrupt_urgent:<text>` - same but urgent priority. Returns `"queued (urgent)"`.
- `history` - returns the session's message history as JSON array.

These are sent with the `session_id` field to target a specific session, exactly like the existing `tool:memory` call.

## Hard constraints (violating any of these = rework)

1. No new npm dependencies. Node built-ins only. The dashboard stays two files plus README.
2. Every new endpoint is POST-only, goes through the existing `rejectBadOrigin` check, and uses `readBody` with an explicit size limit.
3. Server binds to 127.0.0.1 only (already the case, keep it).
4. No path traversal: any filename used on disk must be sanitized to a basename with a safe character allowlist.
5. Session IDs from the client must be validated against the pattern `/^session_[a-z0-9]+_\d+_[0-9a-f]+$/i` before being sent to the socket.
6. Keep the polling model. No websockets, no server-sent events, no background timers beyond what exists.
7. All user-facing error strings must be plain English a non-coder understands.
8. Keep files readable. If server.js grows past ~450 lines, factor helpers, but stay one file.

## Phase 1 - Send messages to any live session

### Backend
New endpoint `POST /api/session/message`:
- Body JSON: `{ session_id, text, urgent }` (urgent optional boolean).
- Validate: session_id matches pattern; text is a non-empty string after trim; text <= 8000 chars.
- Find the session's server pipe: reuse `loadLiveSessions` cache (or look up `servers.json` fresh) to map session_id -> server -> debug_socket pipe. If session not found or not live: `{ ok:false, error: "That terminal is not connected right now." }`.
- Send `queue_interrupt:<text>` (or `queue_interrupt_urgent:` when urgent) with the session_id.
- Success response `{ ok:true, queued:true, urgent }`.
- Prefix the text sent to the agent with `[From Michael via dashboard] ` so agents know the source. Do this server-side, not client-side.

### Frontend
On each LIVE session card:
- A single-line text input + Send button + small "urgent" checkbox labeled "interrupt now".
- Enter key sends. While in flight, disable the button. On success show a brief inline "Delivered ✓" that fades; on failure show the error inline in red.
- Offline session cards do not get the input.

## Phase 2 - Share a file with a session

### Backend
New endpoint `POST /api/session/file`:
- multipart is overkill; accept JSON `{ session_id, filename, content_base64, note }`.
- Limits: content_base64 decoded size <= 10 MB (reject larger with a clear message). Use a raised `readBody` limit (16 MB) for THIS endpoint only.
- Sanitize filename: strip directories, allow `[A-Za-z0-9._ -]`, collapse anything else to `_`, cap at 100 chars, must not be empty after sanitizing.
- Write to `~/.jcode/dashboard-inbox/<yyyy-mm-dd>/<sanitized-name>`. If a file with that name exists, append `-2`, `-3`, ... before the extension. Create dirs as needed.
- Then queue a message to the session (same mechanism as Phase 1, non-urgent unless `urgent` passed):
  `[From Michael via dashboard] I shared a file with you: <absolute path>. <note if provided>. Please read it and take it into account.`
- Respond `{ ok:true, path }`.

### Frontend
On each live session card: a "📎 Share file" button that opens a hidden `<input type=file>`. On pick, read the file client-side with FileReader as base64, show filename + size next to an optional one-line note input and a confirm Send. Files over 10 MB: refuse client-side with a clear message. Show delivered/error state like Phase 1.

## Phase 3 - Plain-English activity view

### Backend
1. Extend `/api/state` sessions with a computed `activity` object per live session (compute server-side so the frontend stays dumb):
   - `headline`: one sentence. Derive: if `is_processing` -> "Working: <first in_progress todo content, or detail field, or 'thinking'>". Else if status ready -> "Idle - waiting for input". Use the session's todos already loaded in `loadTodoBundle`.
   - `state`: one of `working` | `idle` | `offline`.
2. New endpoint `POST /api/session/history`:
   - Body `{ session_id }`, validated as before.
   - Sends the `history` debug command, gets the JSON array, and returns ONLY the last 6 messages, each reduced to `{ role, text }` where text is the concatenated text blocks, truncated to 600 chars each. Strip anything that looks like `<system-reminder>...</system-reminder>` blocks entirely. Tool calls/results become `{ role:"tool", text:"[used tool: <name>]" }` one-liners. This endpoint is called on demand (when the user expands a card), never during the poll loop.

### Frontend
1. Top of page: a "Needs attention" strip listing any live session whose state is `idle` (waiting for you). One line each: name, project folder name, and its headline. Clicking scrolls to the card. Hide the strip when empty.
2. Each session card gets: a colored status dot (green pulsing = working, amber = idle/waiting, gray = offline) and the `activity.headline` as the first line, larger than the metadata.
3. A "Show recent conversation" expander per live card that calls `/api/session/history` when opened and renders the reduced messages as simple chat bubbles (your messages right-aligned, agent left). Re-fetch each time it is opened; no caching.

## Verification you must run and report (evidence, not claims)

1. `node --check server.js` passes.
2. Start the dashboard on PORT=7444 (do not fight the running instance on 7333).
3. With at least one live session present:
   - `curl -s -X POST http://127.0.0.1:7444/api/session/message -H "content-type: application/json" -d "{\"session_id\":\"<real id>\",\"text\":\"test message from dashboard verification, please ignore\"}"` returns `{ok:true,...}`.
   - Invalid session id returns ok:false with a plain-English error and HTTP 400.
   - Oversized text returns a clear error.
   - `/api/session/file` with a small base64 payload writes the file under `~/.jcode/dashboard-inbox/` and returns its path; verify the file exists and content matches.
   - Filename `..\\..\\evil.txt` is sanitized (file lands INSIDE the inbox dir).
   - `/api/session/history` returns at most 6 reduced messages with no `<system-reminder>` content.
   - `/api/state` sessions include `activity` with sensible headline for a working and an idle session.
4. Screenshot-level check: open http://127.0.0.1:7444 in a browser, confirm the strip, dots, inputs render and a real send shows "Delivered ✓". Describe what you saw.
5. Cross-origin guard: `curl -s -X POST http://127.0.0.1:7444/api/session/message -H "Origin: http://evil.example" ...` is rejected 403.

Use the session id of YOUR OWN spawned session or the jaguar session for send tests; keep test messages clearly labeled "please ignore".

## Working style

- Commit per phase with conventional messages (`feat(dashboard): ...`). Do not push.
- If anything in this spec conflicts with what you find in the code, STOP and report back instead of improvising.
- When done, report: what changed per phase, verification evidence per the list above, and any deviations.
