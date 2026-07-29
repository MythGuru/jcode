# jcode Mind Dashboard

A thin, fast, read-only local web dashboard for three existing jcode systems:
Working Memory (STM), Project Knowledge, and Task Graph/plan.

It creates no storage, performs no writes, and uses only local files plus the jcode debug socket.

## Run

```bash
node server.js
```

Open: http://localhost:7333

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

