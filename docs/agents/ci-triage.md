# CI triage

Created: 2026-10-21. Last updated: 2026-11-22.

CI (`.github/workflows/ci.yml`) runs the repository checks from the [README](../../README.md#development) on each OS in its matrix. Each job stops after 60 minutes.

## Jobs and shards

- `lint (<os>)` runs `cargo fmt --check` and `cargo clippy`.
- `test (<os>, <k>/<n>)` builds every test and runs slice `k` of `n` with [`cargo nextest run --partition slice:k/n`](https://nexte.st/docs/ci-features/partitioning/) (nextest 0.9.127 or later). Slices deal the listed tests round-robin, so a test can move to another shard when tests are added or removed. The Node backend step splits the same way.
- Shard 1 of each OS also runs the tests with their own harness and the doc tests.
- Shards: Linux 4, macOS 2, Windows 6. GitHub runs at most 5 macOS jobs at once, and a run has 4: lint, 2 shards and install scripts. The 2026-11-22 main run before the split took 27 minutes on Windows: 7m51 test build, 8m07 nextest run, 6m05 Node backend run. Change a count in the `include` list of the `test` job.
- Only shard 1 of each OS saves the rust-cache entry.
- On macOS, the `test` job builds the lib unit tests (`cargo test --no-run --lib`) before nextest. The `pi-coding-agent` unit tests take 3.7 GB to compile and the lib and the integration test binary 2.8 GB each. macOS runners have 7 GB, so these builds must not overlap.
- The `test` job links the Windows MSVC target with `rust-lld`. x86_64 Linux uses `rust-lld` by default since [Rust 1.90](https://blog.rust-lang.org/2025/09/18/Rust-1.90.0/).

To repeat a failure locally, run the failing test by name. Slice numbers do not match across machines, because the test list differs by OS.

## Read the failure

```sh
gh pr checks <pr> -R yannelli/pi-rust
gh run view <run-id> -R yannelli/pi-rust --json jobs --jq '.jobs[]|[.name,.status,.conclusion,.databaseId]|@tsv'
```

`gh run view --log-failed` works only after the full run is complete. You can get the log of a completed job while other jobs continue to run:

```sh
gh api --allow-escape-sequences repos/yannelli/pi-rust/actions/jobs/<job-id>/logs \
  | sed 's/\x1b\[[0-9;]*m//g' > job.log
grep -nE 'panicked|FAILED|timed out|left:|right:|error(\[|:)' job.log
```

If a job runs much longer than usual, it is hung. To get its logs before the timeout, cancel the run (`gh run cancel <run-id> -R yannelli/pi-rust`).

## Hangs and flaky tests

- If a pi-js test starts servers or child processes, use `run_cjs_within` from `crates/pi-js/tests/common/mod.rs`. Record each step in the script with `fs.appendFileSync(path.join(__dirname, "stage"), "<step>\n")`. If the script fails or times out, the test prints the last step.
- If a test passes locally and fails on CI, the cause is usually a race. Run it many times in parallel before and after the fix:

```sh
cargo test -p pi-js --test <name> --no-run
bin=$(ls -t target/debug/deps/<name>-* | grep -v '\.d$' | head -1)
for i in $(seq 1 10); do for k in 1 2 3 4 5 6; do ($bin -q >/dev/null 2>&1 || echo fail) & done; wait; done | grep -c fail
```

- Do not retry a flaky test in CI. Do not loosen its assertion. Find the race. Loosen an assertion only when Node itself allows the other result.
- Windows timers have a coarse resolution of about 15 ms. Thus timers a few ms apart often fire in the same wakeup on Windows. To get the same result on Linux, use `#[tokio::test(start_paused = true)]`. Then give the timers delays that line up.

## Windows

Most Windows-only failures to date had one of these causes:

| Symptom | Cause |
| --- | --- |
| Expected `/`, got `\` in a path | Node prints native separators on Windows. Normalize the separators in the test. |
| A named pipe stream never ends | Named pipes cannot be half-closed. pi-js closes the pipe 50 ms after `end()`, as libuv does. |
| `ECONNRESET` after the parent passes a socket to a child | The parent closed its copy before the child imported it. Handles wait for `NODE_HANDLE_ACK`. |
| Code takes the Unix branch in a forked child | A module read `process.platform` at load time, before `process` was built. Read it at the time of use. |
| `fs` error codes are different | Map Windows errors with the libuv table in `crates/pi-durable/src/env/local/`. |

When you change `cfg(windows)` code, cross-build for Windows locally:

```sh
cargo zigbuild --target x86_64-pc-windows-gnu --workspace --tests
```

## Release workflow failures

If a build or daemon job fails, the workflow skips the publish. If the cause is outside the code (a download error, a runner problem), rerun only the failed jobs: `gh run rerun <run-id> -R yannelli/pi-rust --failed`. Otherwise, fix the code in a new PR. Refer to [the release guide](release.md).
