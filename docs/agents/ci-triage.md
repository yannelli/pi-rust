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

`is_utf8` and `lossy_len` in `crates/pi-durable/src/env/utf8.rs` validate command output and measure file lines in the `pi-env` daemon and the host. They have two candidates: a comparison kernel on x86_64 and aarch64, and a shift DFA on every target. The kernel works in 1 KiB blocks: a block of ASCII costs one pass for its largest byte, a block with a few parts that are not ASCII runs the rules on those parts only, and a block full of other text runs only the rule set that its largest and smallest lead bytes allow (8 rule sets). The smallest lead byte comes out of the rule loop itself, so the rules are picked from the block before, and a block whose own bounds do not fit the rules it ran runs again. The kernel is fast only when the compiler turns its loops into vector code. The DFA does not depend on the compiler that way. At the first long input, each candidate is timed against the method the decoder used before (`str::from_utf8` for `is_utf8`, a `utf8_chunks` count for `lossy_len`) on dense text, repeated emoji between ASCII words, a log with a few marks, and ASCII. A candidate replaces that method only when it is no slower on each kind and at least 10% faster on all of them together, so a build without vector loops is slower than it could be, never wrong, and never slower than the old method on those kinds. The check finds such a build before a release. `lint (ubuntu-latest)` runs it with the current stable Rust, which the Release workflow also uses.

```sh
rustup target add <targets>     # once: the targets of release.yml, as listed in the CI step
cargo xtask utf8-asm            # every target of release.yml
cargo xtask utf8-asm --target aarch64-linux-android -- -C opt-level=s   # one target, extra rustc flags
```

The check compiles `utf8.rs` with `opt-level=3` for each target and reads the innermost loops of the kernel's throughput functions, listed in `LOOPS` in `xtask/src/utf8.rs`: the pass for the largest byte (`kernel::valid_prefix`), every instance of the rule sets (`rules::<..>`, 8 instances), the two-byte rules (`two_byte_breaks`), and the parts of sparse two-byte blocks (`parts_break`). Each function must have a loop in which at least half of the instructions use vector registers. The bookkeeping of sparse blocks (`survey`, `pieces_break`) runs once per block and is not listed. Each target gets one line with the instruction count of each vector loop:

```
ok      x86_64-unknown-linux-musl: largest byte 22 (18 vector), rule sets 8 loops of 25 to 70, 2-byte rules 25 (22 vector), 2-byte parts 80 (48 vector)
FAILED  aarch64-linux-android: largest byte 10 (7 vector), rule sets SCALAR (1 of 8), 2-byte rules 18 (14 vector), 2-byte parts 59 (32 vector)
```

The listings are in `target/xtask-utf8/<target>.s`. armv7 has no kernel and always passes; the DFA or `std` validates there.

| Failure | Cause and fix |
| --- | --- |
| `rustc failed: install it with rustup target add <target>` | `release.yml` has a new target. Add it to the CI step. |
| `SCALAR` after a change to `utf8.rs` | The change stopped vectorization. An early exit, a bounds check or a call in the loop can do that. |
| `SCALAR` after a new stable Rust | The new compiler does not vectorize the loop. Compare with the previous compiler (`cargo +<version> xtask utf8-asm`), then change the loop until the check passes again. |
| `NOT FOUND` | A function was renamed or inlined, or a generic one has a different number of instances. Update `LOOPS` in `xtask/src/utf8.rs`. |

`cargo xtask utf8-bench` measures the speed. It builds the ignored `benchmark` test of `utf8.rs` with `opt-level=3` and prints the GB/s of `std`, each candidate, `is_utf8`, the old length count and `lossy_len` on 8 MiB inputs, and what `is_utf8` and `lossy_len` chose. Add rustc flags after `--`: `-C opt-level=s` or `-C opt-level=0` show a build where the kernel is not chosen, and on Apple silicon `--target x86_64-apple-darwin -C target-cpu=x86-64` runs the x86_64 baseline build under Rosetta 2. The inputs include sparse text (ASCII with one character every 256 bytes to 4 KiB), runs of CJK between runs of ASCII, and text that mixes every width.

`cargo xtask utf8-check` builds the same file with `opt-level=3` and runs the ignored `exhaustive` test: about 30 million inputs, on which each candidate must agree with `std` (every sequence of up to four bytes over the boundary bytes of each class, five-byte sequences of one byte per class, at the start and after 8 KiB of ten kinds of text, across block edges, and 60,000 damaged slices of mixed text). It takes about 3 minutes. Run it after a change to `utf8.rs`, on each architecture you can; the dev-profile tests run a smaller cut. It takes the same rustc flags as `utf8-bench`.

## Release workflow failures

If a build or daemon job fails, the workflow skips the publish. If the cause is outside the code (a download error, a runner problem), rerun only the failed jobs: `gh run rerun <run-id> -R yannelli/pi-rust --failed`. Otherwise, fix the code in a new PR. Refer to [the release guide](release.md).
