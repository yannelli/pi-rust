# pi-evals

Behavioral evals for the pi coding agent. This crate ports the upstream private package
`@earendil-works/pi-evals`. It is not published.

Eval suites live in `evals/`:

- `*.docs.eval.rs` files are documentation-lift evals. Each case runs in separate
  `without_docs` and `with_docs` containers, and the report shows the lift.
- Other `*.eval.rs` files are host evals. They run on this machine.

The harness runs the Rust `pi` binary in print mode. Each run gets a new home, agent
directory, workspace, and session. A reload step starts the next prompt in a new `pi`
process. The harness looks for `pi` in `PI_EVAL_PI_BIN`, then next to `pi-evals`, then on
`PATH`.

## Run evals

All evals need `PI_PROVIDER` and `PI_MODEL`, and a credential for that provider in
`~/.pi/agent/auth.json` (or `$PI_CODING_AGENT_DIR/auth.json`).

```bash
cargo build -p pi-coding-agent -p pi-evals
export PI_PROVIDER=openai-codex PI_MODEL=gpt-5.6-sol

# Host evals, then the documentation comparison (the arguments go to `docs`)
target/debug/pi-evals eval

# Host evals only, or one host suite
target/debug/pi-evals host
target/debug/pi-evals host evals/smoke.eval.rs

# Documentation comparison (needs Docker)
target/debug/pi-evals docs --provider openai-codex --model gpt-5.6-sol
target/debug/pi-evals docs evals/extensions.docs.eval.rs --runs-per-variant 5
target/debug/pi-evals docs -t "adds the model"
```

`PI_EVAL_RUNS_PER_VARIANT=5` is the same as `--runs-per-variant 5`.

The comparison builds the two images from `docker/Dockerfile`, with the repository root as
the build context. Both images contain the same `pi` binary. Only `with_docs` contains the
repository `README.md` and `CHANGELOG.md`, and only `with_docs` keeps the documentation
section of the default system prompt. Documentation evals allow only `read`, `write`,
`edit`, `grep`, `find`, and `ls`. The runner stays root, and each `pi` process runs as
UID 65532. The `pi-evals` binary, which holds the eval definitions, judges, and fixtures,
is readable only by root.

Results go to `crates/pi-evals/.eval/<timestamp>_<id>/`: `protocol.json`,
`expected-runs.json`, `observations.jsonl`, `tasks/*/vitest.json`,
`<variant>/sessions/*/session.jsonl`, `report.json`, and `report.txt`. The per-arm report
uses the Vitest JSON shape that upstream reads.

## Tests

```bash
cargo test -p pi-evals
```

The tests use no network access and no real provider.
