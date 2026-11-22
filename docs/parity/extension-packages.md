# Compatibility of 25 published Pi extensions

This audit covers the 25 most downloaded extensions in `narumiruna/pi-extensions`, a maintained collection of independently published packages for the upstream Pi coding agent. The source snapshot is `a2d1b5d4456acdcc327b0a5cf920a029eabc0877`. Each eligible package had to declare `pi.extensions` in both its repository manifest and its published npm manifest. There were 38 eligible packages. [Extension repository at the audited commit](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877)

The ranking uses npm downloads per package, across all versions, from October 23 through November 21, 2026, inclusive. The tested versions were the npm `latest` versions on November 22. Each package had its own installation prefix. The audit checked the installed version and tarball integrity against npm metadata. [npm download-count API](https://github.com/npm/registry/blob/main/docs/download-counts.md) · [Download count for the first-ranked package](https://api.npmjs.org/downloads/point/2026-10-23:2026-11-21/@narumitw/pi-plan-mode)

## Package checks

All 25 packages passed their local checks on macOS arm64. Each check loaded the published package directory with `pi -e`. The CLI used the default QuickJS runtime for ranks 1 through 23. Chat and AgentFS used `--extension-runtime auto` with Node 22.23.5. All package names below use the `@narumitw/` scope.

| Rank | Package | Version | Downloads | Behavior checked locally |
| ---: | --- | --- | ---: | --- |
| 1 | [pi-plan-mode](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-plan-mode) | 0.60.3 | 39,245 | `/plan start` enabled plan mode. |
| 2 | [pi-goal](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-goal) | 0.55.3 | 31,294 | `/goal status` reported an empty goal. |
| 3 | [pi-usage](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-usage) | 0.69.3 | 30,727 | `/usage` opened the provider usage menu. |
| 4 | [pi-btw](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-btw) | 0.63.8 | 23,959 | `/btw` opened the side-thread view in a terminal. |
| 5 | [pi-lsp](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-lsp) | 0.49.12 | 9,953 | `/lsp` reported local adapter configuration. |
| 6 | [pi-firecrawl](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-firecrawl) | 0.51.1 | 6,365 | `/firecrawl status` listed the available capability tools. |
| 7 | [pi-sync](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-sync) | 0.62.0 | 6,125 | The session started cleanly and `/sync help` displayed the sync commands. |
| 8 | [pi-statusline](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-statusline) | 0.51.1 | 5,177 | `/statusline status` reported the built-in configuration. |
| 9 | [pi-starship](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-starship) | 0.67.3 | 4,951 | `/starship status` reported the active configuration. |
| 10 | [pi-codex-compact](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-codex-compact) | 0.56.5 | 4,870 | `/codex-compact` reported the compaction settings path. |
| 11 | [pi-subagents](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-subagents) | 4.0.1 | 4,517 | `/subagents settings` reported the user settings path. |
| 12 | [pi-caffeinate](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-caffeinate) | 0.49.15 | 3,017 | `/caffeinate status` honored the disabled setting. |
| 13 | [pi-accounts](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-accounts) | 0.54.3 | 2,687 | Opened the account manager, then selected, recorded and cleared a fixture account. |
| 14 | [pi-stamp](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-stamp) | 0.54.2 | 2,220 | `/stamp` opened the timestamp presets menu. |
| 15 | [pi-progress](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-progress) | 0.8.0 | 1,892 | `/progress clear` cleared the session's progress widget. |
| 16 | [pi-herdr](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-herdr) | 0.6.0 | 1,881 | `/herdr` opened its widget settings with a fixture pane ID. |
| 17 | [pi-notify](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-notify) | 0.4.4 | 1,795 | `/notify` opened the destinations menu in a terminal. |
| 18 | [pi-worktree](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-worktree) | 0.51.10 | 1,735 | `/worktree` listed worktrees in a temporary Git repository. |
| 19 | [pi-codex-imagegen](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-codex-imagegen) | 0.2.3 | 1,608 | `/codex-imagegen` opened its endpoint settings in a terminal. |
| 20 | [pi-ticker](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-ticker) | 0.1.8 | 1,550 | `/ticker help` displayed usage with an empty watchlist. |
| 21 | [pi-chrome-devtools](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-chrome-devtools) | 0.54.1 | 1,422 | `/chrome-devtools status` reported the available browser tools. |
| 22 | [pi-notes](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-notes) | 0.2.6 | 1,344 | `/notes` opened the note picker in a terminal. |
| 23 | [pi-tool](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-tool) | 0.4.3 | 1,313 | `/tool` opened the tool browser. |
| 24 | [pi-chat](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-chat) | 0.1.13 | 1,209 | `/chat` offered to create a private room in a terminal. |
| 25 | [pi-agentfs](https://github.com/narumiruna/pi-extensions/tree/a2d1b5d4456acdcc327b0a5cf920a029eabc0877/packages/pi-agentfs) | 0.2.6 | 1,203 | Wrote, read and edited a virtual file, then read it through virtual Bash. The updated bytes were in SQLite and the host file stayed absent. Its host-shell guard also fired. |

The checks used temporary homes and agent directories. The five terminal checks opened the package UI through a pseudoterminal. Accounts selected and cleared both a synthetic OpenRouter API key and a synthetic Codex OAuth credential. AgentFS used a loopback model fixture to request its tools. Live account login, hosted services, browser control, peer connections, background jobs, language-server sessions, desktop notifications and activity inhibitors were outside these checks.

## Host gaps found

| Before the fix | Change |
| --- | --- |
| Passing the package directory to `-e` missed the declared entry point for all 25 packages. | Resolve package manifests and their declared resource paths through the package manager. Temporary npm and Git sources use its cache and preserve their source on reload. |
| Sync lost its session at startup. Progress and several menu commands lost state between callbacks. These packages use `ctx.sessionManager` as a map key. | Reuse the manager object across callbacks for the same session. A replacement session gets its own object. |
| Accounts could open its menu while also reporting that it could not persist the selection. Named accounts need session writes and runtime credentials. | Implement the concrete manager's custom-entry writer and bind runtime keys, provider configuration and the provider's OAuth methods to the selected session. |
| Chat's menu and AgentFS needed native addons in npm dependencies installed above their package directories. The automatic runtime selector missed those dependencies. | Follow installed runtime dependencies when choosing an engine. QuickJS remains the default. Set `--extension-runtime auto` or `node` for these two packages. |
| AgentFS could load on Node but failed when it tried to supply its own filesystem operations to the read, write and edit tools. | Pass the callbacks to the existing Rust tool implementations. File content and mutations go through the extension's filesystem. |

The CLI also loaded `-e npm:@narumitw/pi-goal@0.55.3` into a fresh temporary package directory. A second run with `--offline` reused that installation. Both runs registered the published entry point and returned the expected response to `/goal status`.

## Windows checks

All 25 packages, the five terminal menus and the AgentFS check also passed on Windows 11 x64 build 26300 with Node 26.13.0. The CLI was a debug `x86_64-pc-windows-gnu` build; releases use MSVC. Package prefixes and temporary directories had spaces in their paths, and the terminal checks ran in a Windows pseudoconsole. The Windows run found two more host gaps:

| Before the fix | Change |
| --- | --- |
| Accounts failed to load. Its `fchmodSync` call on a descriptor opened for reading threw `EACCES`. | Reopen the handle with attribute access, as libuv does, so `fchmod` sets the read-only attribute through any descriptor. |
| In RPC, JSON and print modes, `-e npm:` wrote npm's output into the protocol stream on stdout, and npm inherited the protocol's stdin. Unix redirects the stdout descriptor, but Windows does not. | While pi owns stdout, run package manager commands with stdin closed and stdout sent to stderr, as upstream does. |

The CLI also loaded a local package given as an absolute path with backslashes or forward slashes, as a `.\` relative path and as an entry file. It loaded `-e npm:@narumitw/pi-goal@0.55.3` online and again with `--offline`.

The Node backend starts more slowly on Windows. In the debug build, Chat drew its first frame after about 20 seconds; a QuickJS package took about 2 seconds. On Windows CI, the Node backend tests also take about twice as long as on macOS and Linux. The check scripts allow 60 to 90 seconds for each package.

## Final macOS build

The final debug CLI had SHA-256 `26124f22dbd015c9a2f85aa562abbe8b7aa226d37e5d537ab73f908c1fead222`. All 1,935 tests in `pi-coding-agent` passed. Five regression tests for sessions, accounts and file operations passed again with Node selected explicitly. Rust formatting and workspace Clippy passed. The macOS linker emitted the same compact-unwind size warning on the baseline and final debug builds.

## Repeat the checks

From the repository root, build the local CLI and install the pinned npm packages in a temporary directory. Pi supplies the extension SDK. The install command omits peer dependencies and package lifecycle scripts. npm resolves each package's dependency ranges at install time. Use Node 22 or later and Python 3.9 or later with SQLite. The terminal check uses a POSIX pseudoterminal, or a pseudoconsole on Windows. On Windows, run the commands in Git Bash and use `py -3` for `python3`. The scripts add `.exe` to `target/debug/pi`.

```sh
cargo build --locked -p pi-coding-agent --bin pi
prefixes="$(mktemp -d)"
node scripts/check-upstream-extensions.mjs --list | while IFS= read -r spec; do
  name="${spec#@narumitw/}"
  npm install --prefix "$prefixes/${name%@*}" --ignore-scripts --legacy-peer-deps \
    --no-audit --no-fund "$spec" || exit 1
done
node scripts/check-upstream-extensions.mjs target/debug/pi "$prefixes"
python3 scripts/check-upstream-extension-menus.py target/debug/pi "$prefixes"
python3 scripts/check-upstream-agentfs.py target/debug/pi "$prefixes"
```

The RPC check verifies that the command came from the package's declared entry point and checks its output. A TUI-only command should report its mode requirement over RPC. The separate terminal check verifies that the same command opens the menu. Each script exits with an error if an assertion fails.
