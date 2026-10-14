FROM rust:1-slim-bookworm AS builder
WORKDIR /work
# Build against an ephemeral repository mount. Only outputs staged under
# /opt/evaluator persist; the Cargo target directory is a build cache.
RUN --mount=type=bind,source=.,target=/repo \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --manifest-path /repo/Cargo.toml --target-dir /work/target --release --locked \
      -p pi-coding-agent -p pi-evals \
    && install -D -m 0755 /work/target/release/pi /opt/evaluator/bin/pi \
    && install -D -m 0700 /work/target/release/pi-evals /opt/evaluator/bin/pi-evals \
    && install -D -m 0644 /repo/README.md /opt/evaluator/docs/README.md \
    && install -D -m 0644 /repo/CHANGELOG.md /opt/evaluator/docs/CHANGELOG.md

# Both variants use the same pi binary. The control omits only the documentation
# whose effect the experiment measures.
FROM debian:bookworm-slim AS evaluator
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /opt/evaluator/bin/pi /usr/local/bin/pi
COPY --from=builder /opt/evaluator/bin/pi-evals /opt/pi-evals/pi-evals
# The runner stays root and starts each pi process as UID 65532. Keep the evaluator,
# which embeds the eval definitions, judges, and fixtures, readable only by root.
RUN chown -R root:root /opt/pi-evals \
    && chmod 0700 /opt/pi-evals /opt/pi-evals/pi-evals \
    && mkdir -p /run/pi-eval-secrets \
    && chmod 0700 /run/pi-eval-secrets
WORKDIR /opt/pi-evals
ENTRYPOINT ["/opt/pi-evals/pi-evals", "container"]

FROM evaluator AS without_docs
ENV PI_EVAL_VARIANT=without_docs

FROM evaluator AS with_docs
COPY --from=builder /opt/evaluator/docs /usr/local/share/pi/docs
ENV PI_EVAL_VARIANT=with_docs
