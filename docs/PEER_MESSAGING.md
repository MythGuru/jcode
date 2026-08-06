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
- Each must be a **real git repository of its own**. Two folders inside the same
  repo are not peers.
- Aliases must be unique within a group; directories must be unique everywhere.
- A group needs at least two members.

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
  the server rather than by asking the model nicely.
- A peer message can never appear as though **you** sent it. It is stored and
  displayed with its own role and the sender's alias and project name.
- Only projects listed in the file can talk. Anything else is refused.

## Turning it off

Remove the flag, or set `peer_messaging = false`, and restart. The tool
disappears entirely. Messages already in your history stay truthfully labelled
rather than being rewritten.
