# jcode Mind Dashboard

A thin, fast local web dashboard for three existing jcode systems:
Working Memory (STM), Project Knowledge, and Task Graph/plan.

It reads local files plus the jcode debug socket. From the local dashboard only,
it can also queue a message or shared file into a live session.

## Run

```bash
node server.js
```

Open: http://localhost:7333

## Live cockpit controls

- Live session cards show an activity headline and colored status dot.
- Idle live sessions appear in the Needs attention strip.
- Use the message input to queue text into a live session. Check "interrupt now"
  to use the urgent queue.
- Use "📎 Share file" to copy a file into `~/.jcode/dashboard-inbox/<date>/`
  and tell the selected session where to read it.
- "Show recent conversation" fetches the last few reduced messages on demand.

Optional port override:

```bash
PORT=7334 node server.js
```

## Notes

- Live Working Memory needs a running jcode server with debug control enabled.
  The marker file `~/.jcode/debug_control` enables it without a restart
  (delete the file to disable). Without it, the STM panel shows sessions as
  offline while the knowledge and task-graph panels still work from files.
- A session that is mid-turn (`is_processing`) may time out its STM read;
  the panel shows "STM unavailable" and recovers on a later poll.

