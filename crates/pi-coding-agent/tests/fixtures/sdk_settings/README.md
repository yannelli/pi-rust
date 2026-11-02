# SDK `SettingsManager` oracle fixture

Created 2026-11-01. Last updated 2026-11-01.

`golden.json` is the output of the authentic upstream `SettingsManager` (earendil-works/pi `35dfa757378bd7a8570670e99c2687bdd1039a06`), run on Node v22.23.3 with its lock-pinned packages. It holds 609 cases that call all 139 public members, each in at least 2 cases.

## Provenance
- The oracle run wrote `golden.json` with `JSON.stringify(output, null, 1)`: 5,175,553 bytes, sha256 `08c65fea1538fc3ee7b758cabf1eff27800aba5601f1a9f86e701e93819ac0ab`. An independent second run wrote the same bytes.
- The copy here is the same JSON on one line (4,036,267 bytes, sha256 `dc37cda47a080a4ebe66af708befb1606cafaa048f8bd26b4400a4488b083a7d`). No value, case or key order changed.
- Proof: indented again with one space, the copy is byte-identical to the oracle file. `the_compact_golden_is_the_oracle_output` in `tests/sdk_settings_replay.rs` checks both hashes.
- `oracle/` and `data/` are the generator, copied byte for byte from the prep folder:
  - `oracle/run.mjs` runs the cases and `oracle/harness.mjs` records them;
  - `oracle/hooks.mjs` and `oracle/register.mjs` limit module resolution;
  - `oracle/prepare_run.py` builds and checks the run folder;
  - `oracle/README.md` and `oracle/prep-proof.json` describe the run. `prep-proof.json` is kept on one line; indented again with one space it is the prep file (sha256 `f05ab7de66e259a9c66193dae8ac64b3ef60db5b53eef033b0396988e8b4ce70`);
  - `data/*` hold the case inputs and the manifests of the upstream sources and packages.
  
  The 292 package files are not copied; `data/packages-manifest.json` lists each one with its sha256.

## Replay
`replay.ts` is the case loop of `oracle/run.mjs` as a pi extension command. It imports the same `data/` inputs and `oracle/harness.mjs`, and runs them against pi-rust's SDK `SettingsManager`. `tests/sdk_settings_replay.rs` compares its output with `golden.json`. Every value and key order of all 609 cases must match, with no case skipped or allowed to differ.

## Regenerate
Do not edit `golden.json`. To regenerate it, run `oracle/prepare_run.py` and `oracle/run.mjs` as `oracle/README.md` describes, then write the compact copy and its hashes here.
