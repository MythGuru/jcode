# Peer messaging: turning it on

Peer messaging lets two jcode sessions in **different git repositories** on the
same machine exchange exactly one message and one optional reply, without you
copying text between terminals.

It is **off by default**. Nothing changes until you complete both steps below.

## Step 1: tell jcode which projects may talk

Create `~/.jcode/peer-groups.json` (on Windows,
`C:\Users\<you>\.jcode\peer-groups.json`). A working example lives beside this
file at `docs/peer-groups.example.json`:

```json
{
  "version": 1,
  "groups": [
    {
      "name": "healthview",
      "members": [
        { "alias": "Atlas", "working_dir": "C:\\Users\\micha\\Developer\\healthview-platform" },
        { "alias": "Eve",   "working_dir": "C:\\Users\\micha\\Developer\\healthview-app" }
      ]
    }
  ]
}
```

Rules the loader enforces, with a plain error if you get one wrong:

- Each `working_dir` must be an absolute path that exists.
- Aliases must be unique within a group.
- The same directory may not appear twice, including across different groups.
- A group needs at least two members.

What decides identity is the **exact directory**, not the git repository. A
session is a peer only when its working directory resolves to a path you listed
here, compared after resolving symlinks (and case-insensitively on Windows). The
loader does not check that a directory is a git repo, so two folders inside one
repository would work if you listed them. That is not the intended use, and the
sessions would share a checkout, but nothing stops you.

The practical consequence is the one that matters: **a session can only be a
peer if you personally wrote its directory into this file.** Nothing is
discovered automatically.

The file holds no secrets. It is read **once when the server starts**, so edits
take effect after a restart, never mid-conversation.

If something is wrong, the error names it. The two mistakes that are easiest to
make are a missing key and a misspelled one, and both produce valid JSON, so the
message points at the field rather than at the syntax:

```
Peer groups configuration is invalid: missing required field `working_dir` (line 2, column 62)
Peer groups configuration is invalid: group `healthview` must contain at least two members
Peer groups configuration is invalid: working directory for `Atlas` must be absolute
```

## Step 2: enable the feature

Either set it in `~/.jcode/config.toml`:

```toml
[features]
peer_messaging = true
```

or set the environment variable `JCODE_PEER_MESSAGING_ENABLED=1`.

## Using it

Open a jcode session in each project. Then, in ordinary conversation, one agent
can ask the other something. The sending agent waits while the other reads,
thinks, and optionally replies once. The reply comes back into the same turn.

You see everything in both transcripts.

### Check peers without asking a model

Type `/peers` in any interactive session to open a native status card. This is
ordinary Jcode code, not an AI prompt, so it makes **no model call and uses no
provider tokens**.

The card combines two deliberately separate sources:

- **Live availability** comes from the Jcode server that owns the current
  session. An approved peer is shown as ready, busy, offline, or ambiguous.
- **Recent activity** comes from the existing saved session transcripts for the
  exact current working directory. It shows at most the latest five peer events,
  so it survives a restart without creating a second message database.

The history reader is intentionally bounded to keep `/peers` quick. It considers
only the newest 256 candidate session files, loads at most 12 matching sessions,
reads at most the newest 500 messages from each, and skips any saved session over
2 MiB. When a limit or unreadable file is encountered, the card says that some
older activity was skipped.

Privacy is part of the design. The card may show an approved alias, approved
project name, direction, time, and outcome such as `replied` or `timed out`. It
does **not** show message bodies, reply bodies, full paths, session IDs,
capabilities, message IDs, exchange IDs, or raw configuration errors.

The live overview uses Jcode's existing local-daemon trust boundary. It verifies
that the requested session is currently live and uses that session's pinned peer
identity, but it does not add a new per-request capability. Do not expose the
local daemon socket as a multi-user or network API without adding stronger
request binding first.

The main states are explained in plain language:

- **OFF** means peer messaging is disabled. No transcript history is scanned.
- **CONFIGURATION ERROR** means `peer-groups.json` needs attention. The private
  raw error is not printed in the card, and no history is scanned.
- **Project not listed** means the current exact working directory is not in an
  approved group. Existing local peer history may still be summarized safely.
- **No saved peer activity yet** means the feature is working but no matching
  activity was found.
- **Unable to load peer overview** means the current Jcode server could not be
  reached or did not return a valid overview.

The card also reports whether Ambient Mode is on or off. It does not turn
Ambient Mode on. Even if Ambient Mode is enabled separately, ambient background
work cannot initiate peer messages. This feature therefore leaves the existing
Ambient setting unchanged.

**This needs real interactive sessions.** `jcode run "..."` will not work: a
one-shot command runs its own local agent that holds no live server turn, so the
tool is not offered there at all. That is deliberate. Advertising a tool whose
every call fails would waste tokens and invite the model to keep retrying.

## What it deliberately will not do

- It will not deliver to a **busy** session. You get "Atlas is busy. No message
  was sent." and can retry in the same turn.
- It will not deliver to a session that is **not open**. You get "Atlas is not
  currently available on this jcode server. No message was sent." There is no
  queue and no message waiting in the dark.
- An alias that is not in your group is refused by name: "Atlantis is not a
  member of your peer group."
- A session cannot message **itself**.
- It cannot start a **loop**. One message causes at most one reply, enforced by
  the server rather than by asking the model nicely. Tested by instructing both
  agents to volley back and forth indefinitely: the sender's second attempt was
  refused with "This normal turn has already started a peer exchange", and the
  recipient trying to peer back found the sender busy. Two barriers, not one.
  The whole exercise cost 2 peer calls per side and ended in 39 seconds.
- A peer message can never appear as though **you** sent it. In the stored
  session it carries `display_role: peer` and reads "Verified peer message from
  Eve (`repo-eve`)", with the sender's alias and project name. Your own messages
  carry no such marker, so the two cannot be confused after the fact.
- Only projects listed in the file can talk. Anything else is refused.

## Turning it off

Remove the flag, or set `peer_messaging = false`, and restart. The tool
disappears entirely. Messages already in your history stay truthfully labelled
rather than being rewritten.
