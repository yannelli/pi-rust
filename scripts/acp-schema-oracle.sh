#!/bin/sh
# Check or regenerate the ACP wire fixtures with the original schema.
set -eu

case "${1-}" in
    ''|--write) ;;
    *) echo "Usage: sh scripts/acp-schema-oracle.sh [--write]" >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { echo "Expected at most one argument" >&2; exit 2; }

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cases="$repo/crates/pi-coding-agent/src/acp/tests"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/src"
cp "$cases/schema_outbound.rs" "$cases/schema_inbound.rs" "$scratch/src/"
cp "$repo/Cargo.lock" "$scratch/Cargo.lock"
cat > "$scratch/Cargo.toml" <<'TOML'
[package]
name = "acp-schema-oracle"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
agent-client-protocol-schema = { version = "=0.13.7", features = ["unstable_auth_methods", "unstable_end_turn_token_usage"] }
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order", "float_roundtrip", "unbounded_depth"] }
TOML
cat > "$scratch/src/main.rs" <<'RS'
mod cases {
    use agent_client_protocol_schema::*;
    include!("schema_outbound.rs");
    include!("schema_inbound.rs");
}

fn main() {
    let mut fixtures = serde_json::Map::new();
    for (name, value) in cases::outbound().into_iter().chain(cases::inbound()) {
        assert!(fixtures.insert(name.into(), value.into()).is_none(), "duplicate case {name}");
    }
    println!("{}", serde_json::to_string_pretty(&fixtures).unwrap());
}
RS
cargo run --quiet --manifest-path "$scratch/Cargo.toml" > "$scratch/golden.json"
if [ "${1-}" = --write ]; then
    cp "$scratch/golden.json" "$cases/schema_golden.json"
else
    diff -u "$cases/schema_golden.json" "$scratch/golden.json"
fi
