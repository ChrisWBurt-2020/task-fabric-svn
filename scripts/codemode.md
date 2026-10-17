# codemode (experimental)

Batch read-only workspace inspection into a single execution instead of N
sequential tool calls.

## When to use it

Inventory-style work: finding files, reading several files to filter or
synthesize, checking many small facts at once. If you catch yourself about
to make 3+ consecutive read/glob/grep calls, write a codemode script instead.

Do NOT use it for: writing files (use your normal edit flow), running
commands, network access. v1 is read-only by design.

## How

1. Write the script to `/tmp/codemode-<short-name>.js` with your file tools.
   Keep it under ~200 lines. It runs with Deno; available APIs are the
   Deno namespace for file reading (`Deno.readDir`, `Deno.readTextFile`,
   `Deno.stat`) plus standard JS. No network, no env, no subprocess.

2. Run it:
   ```
   /opt/task_fabric_svn/codemode /tmp/codemode-<short-name>.js
   ```

3. The wrapper prints the script's stdout (cap 64KB). Parse the JSON and
   continue. One execution, one roundtrip.

## Script contract

- Read from `/workspace` (the task snapshot) and `/tmp` only.
- Print a single JSON object to stdout: the *synthesis*, not raw dumps.
- Exit 0 on success, non-zero with a short stderr message on failure.
- Wall clock cap is 20s; keep scripts fast and bounded (slice large
  listings, cap per-file reads).

## Example

```js
// /tmp/codemode-inventory.js — which .py files mention "ledger"?
const hits = [];
for await (const e of Deno.readDir("/workspace/scripts")) {
  if (!e.isFile || !e.name.endsWith(".py")) continue;
  const text = await Deno.readTextFile(`/workspace/scripts/${e.name}`);
  if (text.includes("ledger")) hits.push(e.name);
}
console.log(JSON.stringify({ py_with_ledger: hits }));
```

Then: `/opt/task_fabric_svn/codemode /tmp/codemode-inventory.js`

## Audit

v1 keeps codemode at parity with every other bash tool: the opencode
session transcript records the exact command, the script content, and its
output. The wrapper itself is read-only, so the containment's normal
finish() workspace diff remains the complete record of what changed —
a codemode run can never produce a change the diff doesn't see. No
separate ledger path is needed because the sandbox is the trust boundary;
the wrapper cannot (and must not) reach the ledger from inside.
