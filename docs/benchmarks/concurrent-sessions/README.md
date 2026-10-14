# Concurrent-session memory measurements

An exploratory comparison of 20 concurrent, non-interactive sessions on one Apple Silicon Mac. These are single-run observations, not a controlled performance benchmark or a claim about all workloads.

## Results

| Measurement | Pi Rust 0.1.9 | Node Pi 0.73.1 | Codex CLI 0.162.1 |
| --- | ---: | ---: | ---: |
| Peak summed RSS (MiB) | 679.88 | 3,431.39 | 3,275.38 |
| Individual main-process peak RSS range (MiB) | 33.52–35.66 | 165.81–181.53 | 162.19–195.91 |
| Mean sampled summed RSS while all 20 sessions were active (MiB) | 581.83 | 2,950.17 | 2,285.51 |
| Batch elapsed time (seconds) | 13.12 | 17.94 | 15.10 |
| Concurrent sessions at the summed RSS peak | 20 | 20 | 19 |
| Exit code 0 | 20/20 | 20/20 | 20/20 |

In these runs, Pi Rust's peak summed RSS was 80.2% lower than Node Pi's (a 5.05× ratio) and 79.2% lower than Codex CLI's (a 4.82× ratio). Both Pi Rust and Codex CLI are Rust-based, so their comparison does not isolate programming-language effects.

## Environment and workload

- macOS 27.2 (build 26B5101f), arm64, hardware identifier `Mac17,4`, 16 GiB RAM.
- Node.js v24.18.0; Node Pi package `@mariozechner/pi-coding-agent@0.73.1` installed in a temporary directory without replacing the Rust executable.
- Each harness launched 20 new processes, using `gpt-6-luna` with low reasoning effort. Pi used provider `openai-codex`; Codex reported provider `openai`.
- Working directory contained an extracted Pi Rust release bundle, its archive, and `SHA256SUMS`. There was no source checkout to explore.
- Runs were sequential: Pi Rust, Codex CLI, then Node Pi. They used real provider requests and were not repeated.
- Pi extensions, skills, prompt templates, and context-file discovery were disabled. Pi Rust also explicitly disabled MCP. Node Pi's listed built-in CLI options did not expose an MCP flag.
- Pi enabled only `read,grep,find,ls`. Codex used a read-only sandbox, no approvals, and no shared daemon.
- Pi sessions were persisted. Codex used its default session persistence. Terminal rendering was not exercised.

All sessions received this prompt:

```text
Exploration-only concurrency test. Inspect the current directory and at most two small documentation files. Only read-only tools; no modifications, credentials, processes, or other agents. Give a brief report and stop.
```

## Measurement method

A Python supervisor launched each batch with `subprocess.Popen`, redirecting output to individual log files. Timing began before the launch loop. Sampling began after all 20 processes had been launched, so early startup peaks may have been missed.

The supervisor read process RSS using `ps`, then slept 250 ms between samples. This is a nominal interval: process-table collection and other supervisor work add time. Exact elapsed sample timestamps are retained in the JSON files.

- Pi Rust: `ps -axo pid=,rss=,%cpu=`; summed RSS of the 20 launched process IDs.
- Node Pi and Codex: `ps -axo pid=,ppid=,rss=,%cpu=`; summed RSS of launched processes and descendants found recursively in the process table.
- RSS values from macOS `ps` are in KiB; reported MiB values divide by 1,024.
- Each process's reported peak is its maximum observed RSS, not an OS-recorded lifetime high-water mark. Individual peaks need not occur simultaneously.
- The mean in the table is an arithmetic mean of samples with all 20 sessions active, not a time-weighted mean.
- Each batch ended once all launched processes exited. All exit codes were zero; no timeout was reached.

## Launch configurations

The following are the common launch arguments for each session, excluding supervisor redirection and the repeated prompt. The supervisor created a separate session path for each Pi process.

Pi Rust:

```sh
pi --provider openai-codex --model gpt-6-luna --thinking low \
  --tools read,grep,find,ls --no-mcp --no-extensions --no-skills \
  --no-prompt-templates --no-context-files \
  --session "$SESSION_FILE" --name "$SESSION_NAME" -p "$PROMPT"
```

Node Pi:

```sh
node "$INSTALL_DIR/node_modules/@mariozechner/pi-coding-agent/dist/cli.js" \
  --provider openai-codex --model gpt-6-luna --thinking low \
  --tools read,grep,find,ls --no-extensions --no-skills \
  --no-prompt-templates --no-context-files \
  --session "$SESSION_FILE" -p "$PROMPT"
```

Node Pi used a temporary `PI_CODING_AGENT_DIR`, existing OAuth credentials, and `PI_TELEMETRY=0`. Its bundled catalog did not list Luna, so the temporary `models.json` registered this model:

```json
{
  "providers": {
    "openai-codex": {
      "baseUrl": "https://chatgpt.com/backend-api",
      "api": "openai-codex-responses",
      "models": [{
        "id": "gpt-6-luna",
        "name": "GPT-6 Luna",
        "api": "openai-codex-responses",
        "reasoning": true,
        "input": ["text", "image"],
        "contextWindow": 272000,
        "maxTokens": 128000,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
      }]
    }
  }
}
```

The zero costs are placeholders for local accounting, not a statement about provider pricing. The temporary credential copy was removed after the run; credentials and session content are not included here.

Codex CLI:

```sh
codex --no-daemon -a never exec --ignore-user-config --ignore-rules \
  --skip-git-repo-check --sandbox read-only --model gpt-6-luna \
  -c 'model_reasoning_effort="low"' --color never "$PROMPT"
```

## Limitations

- Summing RSS can count shared pages more than once. These values are not unique physical memory, macOS memory pressure, or proportional set size.
- Sampling can miss short-lived peaks and child processes. Rust's measurement excluded descendants; Node Pi and Codex included them. Node Pi had exactly 20 processes at its measured combined peak; Codex had 22.
- Tool behavior and completed work differed. Some Node Pi sessions reported unavailable documentation search tools and stopped without reading documentation. Rust sessions also varied in which files they read.
- At least one Codex session declined exploration because it interpreted the prompt's prohibition on processes as excluding shell commands. Exit code zero therefore does not prove equivalent task completion.
- Codex still loaded local hooks despite ignoring user config and rules. Its first log reported a SessionEnd hook timeout clamp and a model-catalog refresh timeout. This configuration was not fully isolated.
- Rust Pi used existing configuration; Node Pi used a temporary configuration. Disabling discovery features reduces differences but does not establish identical configuration or system prompts.
- API latency, provider behavior, filesystem caches, startup work, and session writes can affect elapsed time. These runs do not establish a speedup in inference or harness processing.
- This small documentation task does not measure long contexts, large tool outputs, interactive rendering, plugins, or long-running agent sessions.

A stronger follow-up would use identical local mock-provider responses, a fixed sequence of successful tool calls, matching process-tree accounting, and repeated runs with startup sampling already active.

## Data files

- [Pi Rust samples](pi-rust.json)
- [Node Pi samples](pi-node.json)
- [Codex CLI samples](codex.json)

The files retain measured timestamps, RSS, CPU readings, process IDs, summaries, and exit codes. Machine-local log/session paths were removed. Model transcripts and credentials are excluded. CPU readings are retained as collected but are not used to claim CPU utilization improvements.
