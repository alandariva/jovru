# `jjp/1` example program

`export.js` is a dependency-free Node.js (18 or later) program that speaks
`jjp/1`, the protocol Jykre Studio's automation uses to give an external
program its inputs and take its outputs back
([specification](../../docs/plans/automation-engine-specification.md),
section 9.2). It writes synthetic rows to a CSV file and returns how many it
wrote and where.

## As a job

| Field | Value |
| --- | --- |
| `program` | `node` |
| `arguments` | the absolute path of `export.js` |
| `entrypoint` | the same path (a published version pins its SHA-256) |
| `protocol` | `jjp/1` |
| `inputs` | `rows` (number, optional, default 10), `file` (text, optional) |
| `outputs` | `rows` (number), `file` (text) |

## What it does

1. Reads its token from the first line of **stdin**,
   `{"jjp":"1","token":"…"}`, up to end of file. Stdin is reserved for it.
2. Connects to `JYKRE_JJP_ADDRESS` (`127.0.0.1:<port>`) and sends `hello`
   with the token. The answer carries the run's inputs (text or `null`),
   the milliseconds left before the deadline and the longest line allowed
   (1 MiB).
3. Sends `log` and `progress` notifications while it works, and stops
   with `fail` code `CANCELLED` when the host sends `cancel`.
4. Sends `complete` with every declared output, as text, then exits 0.

Messages are one JSON-RPC 2.0 object per line. Values are always strings,
never JSON numbers, so nothing passes through floating point.

## Rules worth knowing

- The program runs with your privileges; Studio supervises it, it does not
  sandbox it. It inherits Studio's environment except `JYKRE_*`, `OTEL_*`,
  `NODE_OPTIONS`, `PYTHONPATH`, `PYTHONSTARTUP` and `PYTHONHOME`.
- stdout, stderr and `log` messages are kept on this computer for 7 days,
  at most 1 MiB per stream: never print passwords, tokens or personal data.
- **Stop** and the deadline send `cancel`; the whole process tree is killed
  5 s later if it is still running.

The test `automation::process::tests::the_node_example_exports_and_completes`
runs it end to end when Node.js is on `PATH`:

```bash
cargo test --lib automation::process::tests::the_node_example -- --ignored
```
