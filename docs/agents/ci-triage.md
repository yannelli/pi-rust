# CI triage

CI (`.github/workflows/ci.yml`) runs the repository checks from the [README](../../README.md#development) on each OS in its matrix. Each job stops after 60 minutes.

## Read the failure

```sh
gh pr checks <pr> -R yannelli/pi-rust
gh run view <run-id> -R yannelli/pi-rust --json jobs --jq '.jobs[]|[.name,.status,.conclusion,.databaseId]|@tsv'
```

`gh run view --log-failed` works only after the whole run finishes. A finished job's log is available while other jobs still run:

```sh
gh api --allow-escape-sequences repos/yannelli/pi-rust/actions/jobs/<job-id>/logs \
  | sed 's/\x1b\[[0-9;]*m//g' > job.log
grep -nE 'panicked|FAILED|timed out|left:|right:|error(\[|:)' job.log
```

A job that runs much longer than usual is hung. Cancel the run (`gh run cancel <run-id> -R yannelli/pi-rust`) to get its logs instead of waiting for the timeout.

## Hangs and flaky tests

- pi-js tests that start servers or child processes should use `run_cjs_within` from `crates/pi-js/tests/common/mod.rs`. Mark progress in the script with `fs.appendFileSync(path.join(__dirname, "stage"), "<step>\n")`; a timeout or script failure prints the last step.
- A test that passes locally and fails on CI is usually a race. Stress it before and after the fix:

```sh
cargo test -p pi-js --test <name> --no-run
bin=$(ls -t target/debug/deps/<name>-* | grep -v '\.d$' | head -1)
for i in $(seq 1 10); do for k in 1 2 3 4 5 6; do ($bin -q >/dev/null 2>&1 || echo fail) & done; wait; done | grep -c fail
```

- Do not retry a flaky test in CI or loosen its assertion. Find the race. Relax an assertion only when Node itself allows the other result.
- Windows timers are coarse (about 15 ms), so timers a few ms apart often fire in the same wakeup there. To get the same on Linux, use `#[tokio::test(start_paused = true)]` and give the timers delays that line up.

## Windows

Most Windows-only failures so far had one of these causes:

| Symptom | Cause |
| --- | --- |
| Expected `/`, got `\` in a path | Node prints native separators on Windows; normalize in the test |
| A named pipe stream never ends | Named pipes cannot be half-closed; pi-js closes the pipe 50 ms after `end()`, as libuv does |
| `ECONNRESET` after passing a socket to a child | The parent closed its copy before the child imported it; handles wait for `NODE_HANDLE_ACK` |
| Code takes the Unix branch in a forked child | A module read `process.platform` at load time, before `process` was built; check it when used |
| `fs` error codes differ | Map Windows errors with the libuv table in `crates/pi-durable/src/env/local/` |

Cross-build Windows locally when you change `cfg(windows)` code:

```sh
cargo zigbuild --target x86_64-pc-windows-gnu --workspace --tests
```

## Release workflow failures

A failed build or daemon job skips the publish. If the cause is outside the code (a download error, a runner problem), rerun only the failed jobs: `gh run rerun <run-id> -R yannelli/pi-rust --failed`. Otherwise fix the code in a new PR. See [the release guide](release.md).
