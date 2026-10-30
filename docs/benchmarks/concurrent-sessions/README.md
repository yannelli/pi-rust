# Concurrent-session memory measurements

This report is an exploratory comparison of 20 concurrent, non-interactive sessions on one Apple Silicon Mac. Each result comes from one run. The report is not a controlled performance benchmark, and it makes no claim about all workloads.

## Results

| Measurement | Pi Rust 0.1.9 | Node Pi 0.73.1 | Codex CLI 0.162.1 |
| --- | ---: | ---: | ---: |
| Peak summed RSS (MiB) | 679.88 | 3,431.39 | 3,275.38 |
| Individual main-process peak RSS range (MiB) | 33.52–35.66 | 165.81–181.53 | 162.19–195.91 |
| Mean sampled summed RSS while all 20 sessions were active (MiB) | 581.83 | 2,950.17 | 2,285.51 |
| Batch elapsed time (seconds) | 13.12 | 17.94 | 15.10 |
| Concurrent sessions at the summed RSS peak | 20 | 20 | 19 |
| Exit code 0 | 20/20 | 20/20 | 20/20 |

In these runs, the peak summed RSS of Pi Rust was 80.2% lower than Node Pi (a 5.05× ratio). It was 79.2% lower than Codex CLI (a 4.82× ratio). Pi Rust and Codex CLI both use Rust, so their comparison does not isolate the effects of the programming language.

## Environment and workload

- macOS 27.2 (build 26B5101f), arm64, hardware identifier `Mac17,4`, 16 GiB RAM.
- Node.js v24.18.0. The Node Pi package `@mariozechner/pi-coding-agent@0.73.1` was installed in a temporary directory. The installation did not replace the Rust executable.
- Each harness launched 20 new processes. Each process used `gpt-6-luna` with low reasoning effort. Pi used the provider `openai-codex`. Codex reported the provider `openai`.
- The working directory contained an extracted Pi Rust release bundle, its archive, and `SHA256SUMS`. It contained no source checkout.
- The runs were sequential: Pi Rust, Codex CLI, then Node Pi. They used real provider requests and were not repeated.
- The Pi runs disabled extensions, skills, prompt templates, and context-file discovery. Pi Rust also disabled MCP explicitly. The built-in CLI options that Node Pi lists did not include an MCP flag.
- Pi enabled only `read,grep,find,ls`. Codex used a read-only sandbox, no approvals, and no shared daemon.
- Pi saved its sessions. Codex used its default session persistence. The runs did not use terminal rendering.

All sessions received this prompt:

```text
Exploration-only concurrency test. Inspect the current directory and at most two small documentation files. Only read-only tools; no modifications, credentials, processes, or other agents. Give a brief report and stop.
```

## Measurement method

A Python supervisor launched each batch with `subprocess.Popen`. It sent the output of each process to a separate log file. The timer started before the launch loop. The supervisor started to sample after it launched all 20 processes. Thus the samples do not include startup peaks before that time.

The supervisor read the process RSS with `ps`. Then it slept 250 ms between samples. The 250 ms interval is nominal, because the collection of the process table and other supervisor work add time. The JSON files keep the exact elapsed timestamp of each sample.

- Pi Rust: `ps -axo pid=,rss=,%cpu=`. The supervisor summed the RSS of the 20 launched process IDs.
- Node Pi and Codex: `ps -axo pid=,ppid=,rss=,%cpu=`. The supervisor summed the RSS of the launched processes and their descendants. It found the descendants recursively in the process table.
- RSS values from macOS `ps` are in KiB. The reported MiB values are the KiB values divided by 1,024.
- The reported peak of each process is its maximum observed RSS. It is not the lifetime high-water mark that the OS records. The individual peaks can occur at different times.
- The mean in the table is the arithmetic mean of the samples with all 20 sessions active. It is not weighted by time.
- Each batch ended when all launched processes exited. All exit codes were zero. No timeout occurred.

## Launch configurations

These are the common launch arguments for each session. They do not include the supervisor redirection or the repeated prompt. The supervisor created a separate session path for each Pi process.

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

The zero costs are placeholders for local accounting. They do not state provider pricing. The temporary copy of the credentials was removed after the run. This report does not include credentials or session content.

Codex CLI:

```sh
codex --no-daemon -a never exec --ignore-user-config --ignore-rules \
  --skip-git-repo-check --sandbox read-only --model gpt-6-luna \
  -c 'model_reasoning_effort="low"' --color never "$PROMPT"
```

## Limitations

- A sum of RSS values can count shared pages more than once. These values do not measure unique physical memory, macOS memory pressure, or proportional set size.
- Samples can miss short-lived peaks and child processes. The Pi Rust measurement excluded descendants. The Node Pi and Codex measurements included them. Node Pi had exactly 20 processes at its measured combined peak. Codex had 22.
- The tool behavior and the completed work were different. Some Node Pi sessions reported that documentation search tools were not available. Those sessions stopped and did not read documentation. The Pi Rust sessions did not all read the same files.
- At least one Codex session refused to explore. It read the prohibition on processes in the prompt as a prohibition on shell commands. Thus exit code zero does not prove that the sessions completed equivalent tasks.
- Codex loaded local hooks, although it ignored user config and rules. Its first log reported a SessionEnd hook timeout clamp and a model-catalog refresh timeout. Thus the Codex configuration was not fully isolated.
- Pi Rust used the existing configuration. Node Pi used a temporary configuration. With discovery features disabled, there are fewer differences. This does not prove that the configurations or the system prompts are identical.
- API latency, provider behavior, filesystem caches, startup work, and session writes can change the elapsed time. These runs do not prove a speedup in inference or in the work of the harness.
- This small documentation task does not measure long contexts, large tool outputs, interactive rendering, plugins, or agent sessions that run for a long time.

A stronger follow-up test needs these conditions:

- Identical responses from a local mock provider.
- A fixed sequence of successful tool calls.
- The same process-tree accounting for each harness.
- Repeated runs, with the sampler active before the processes start.

## Data files

- [Pi Rust samples](pi-rust.json)
- [Node Pi samples](pi-node.json)
- [Codex CLI samples](codex.json)

The files keep the measured timestamps, RSS, CPU values, process IDs, summaries, and exit codes. Machine-local log and session paths were removed. The files do not include model transcripts or credentials. The CPU values are as collected. This report does not use them to claim CPU utilization improvements.
