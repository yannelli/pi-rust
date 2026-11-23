# CI triage

Created: 2026-10-21. Last updated: 2026-11-23.

CI (`.github/workflows/ci.yml`) runs the repository checks from the [README](../../README.md#development) on each OS in its matrix. Each job stops after 60 minutes.

## Jobs and shards

- `lint (<os>)` runs `cargo fmt --check` and `cargo clippy`. On Linux it also runs the [UTF-8 kernel assembly check](#utf-8-kernel-assembly-check). On macOS it also runs `scripts/test-install.sh`.
- `test (<os>, <k>/<n>)` builds every test and runs slice `k` of `n` with [`cargo nextest run --partition slice:k/n`](https://nexte.st/docs/ci-features/partitioning/) (nextest 0.9.127 or later). Slices deal the listed tests round-robin, so a test can move to another shard when tests are added or removed. The Node backend step splits the same way.
- Shard 1 of each OS also runs the tests with their own harness and the doc tests.
- Shards: Linux 4, Windows 6. macOS splits by target: `test (macos-latest, unit 1/1)` runs the lib unit tests (`--workspace --exclude pi-evals --lib`), and `test (macos-latest, integration k/2)` runs the bin and integration tests (`--workspace --bins --test '*'`). Integration shard 2 runs all Node backend tests. GitHub runs at most 5 macOS jobs at once, and a run has 4: lint and 3 test jobs. The 2026-11-22 main run before the split took 27 minutes on Windows: 7m51 test build, 8m07 nextest run, 6m05 Node backend run. Change a count in the `include` list of the `test` job.
- Only shard 1 of each OS saves the rust-cache entry (on macOS, integration shard 1).
- macOS runners have 3 CPUs, 7.5 GB of memory and no swap. With a full cache hit, the macOS test build took 2m38 to 15m09 on 2026-11-22 and 23, so the macOS jobs usually finish last. The `pi-coding-agent` lib unit tests take 3.7 GB to compile, and the lib and its integration test binary 2.8 GB each; the target split keeps the first apart from the other two. Each macOS test job prints its CPU model, CPU count and memory, and a `vm_stat` sample every 30 seconds.
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

## UTF-8 kernel assembly check

`is_utf8` in `crates/pi-durable/src/env/utf8.rs` validates command output and file lines in the `pi-env` daemon and the host. Its kernel is faster than `std` only when the compiler turns the kernel's loops into vector code. At the first long input, `is_utf8` times the kernel against `std` and keeps the faster one, so a build without vector loops is slow, not wrong. The check finds such a build before a release. `lint (ubuntu-latest)` runs it with the current stable Rust, which the Release workflow also uses.

```sh
rustup target add <targets>     # once: the targets of release.yml, as listed in the CI step
cargo xtask utf8-asm            # every target of release.yml
cargo xtask utf8-asm --target aarch64-linux-android -- -C opt-level=s   # one target, extra rustc flags
```

The check compiles `utf8.rs` with `opt-level=3` for each target and reads the innermost loops of four functions: the largest-byte pass (`kernel::is_utf8`) and the three rule sets (`block_breaks::<2>`, `<3>` and `<4>`). Each function must have a loop in which at least half of the instructions use vector registers. Each target gets one line with the instruction count of each vector loop:

```
ok      x86_64-unknown-linux-musl: largest byte 7 (4 vector), 2-byte rules 27 (24 vector), 3-byte rules 49 (46 vector), 4-byte rules 85 (82 vector)
FAILED  aarch64-linux-android: largest byte 6 (3 vector), 2-byte rules 20 (16 vector), 3-byte rules 34 (29 vector), 4-byte rules SCALAR
```

The listings are in `target/xtask-utf8/<target>.s`. armv7 has no kernel and always passes.

| Failure | Cause and fix |
| --- | --- |
| `rustc failed: install it with rustup target add <target>` | `release.yml` has a new target. Add it to the CI step. |
| `SCALAR` after a change to `utf8.rs` | The change stopped vectorization. An early exit, a bounds check or a call in the loop can do that. |
| `SCALAR` after a new stable Rust | The new compiler does not vectorize the loop. Compare with the previous compiler (`cargo +<version> xtask utf8-asm`), then change the loop until the check passes again. |
| `NOT FOUND` | A function was renamed or inlined. Update `LOOPS` in `xtask/src/utf8.rs`. |

`cargo xtask utf8-bench` measures the speed. It builds the ignored `benchmark` test of `utf8.rs` with `opt-level=3` and prints the GB/s of `std`, the kernel and `is_utf8` on 8 MiB inputs, and which one `is_utf8` chose. Add rustc flags after `--`: `-C opt-level=s` shows the fallback to `std`, and on Apple silicon `--target x86_64-apple-darwin -C target-cpu=x86-64` runs the x86_64 baseline build under Rosetta 2.

## Release workflow failures

If a build or daemon job fails, the workflow skips the publish. If the cause is outside the code (a download error, a runner problem), rerun only the failed jobs: `gh run rerun <run-id> -R yannelli/pi-rust --failed`. Otherwise, fix the code in a new PR. Refer to [the release guide](release.md).
