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
