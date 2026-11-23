# Settings

Settings load from `<agent dir>/settings.json` (normally `~/.pi/agent/settings.json`). A trusted project's `.pi/settings.json` overrides global values. In a git worktree, see [Git worktrees](#git-worktrees).

## List overflow

`listOverflow` is a pi-rust setting for selectable TUI lists. Set it in `settings.json`, or open `/settings` and change **List overflow**. Menu changes save globally and apply immediately; a trusted project's override takes precedence.

```json
{
  "listOverflow": "wrap"
}
```

| Value | Behavior |
| --- | --- |
| `"truncate"` (default) | Keep one line per item and show `…` when text does not fit. |
| `"wrap"` | Continue long items on following lines, aligned under the item's text. The viewport keeps the selected item together. Mouse clicks on continuation lines select that item. |
| `"scroll"` | Truncate other items. Pause for one second, scroll the selected item's overflowing text horizontally, pause at the end, return to the start, and repeat. |

`PI_REDUCED_MOTION=1` makes `"scroll"` behave as `"truncate"`. The scroll timer runs while an overflowing selected row is rendered and stops when it fits or the list closes.

`listOverflow` applies to these lists:

- Command and extension selection menus.
- Settings and config resources.
- Sessions, the session tree, and user messages.
- Model and provider selectors.
- The extensions browser.

Native extensions can read the same key with `host.settings::<String>("listOverflow")` and fit their own lists.

## Provider identity

`providerIdentity` is a pi-rust setting that chooses how requests to model providers identify the client. Set it in `settings.json`, or open `/settings` and change **Provider identity**. A trusted project's value overrides the global one.

```json
{
  "providerIdentity": "pi-rust"
}
```

| Value | `User-Agent` | Attribution |
| --- | --- | --- |
| `"pi"` (default) | `pi/<upstream version> (<platform>; rust; <arch>)`. The version is the upstream pi release that pi-rust has parity with (`syncedVersion` in [`docs/parity/upstream.json`](../../../docs/parity/upstream.json)). | pi, `https://pi.dev` |
| `"pi-rust"` | `pi-rust/<pi-rust version> (<platform>; rust; <arch>)` | pi-rust, `https://pi-rust.com` |

Attribution headers, such as OpenRouter's `HTTP-Referer` and `X-OpenRouter-Title`, are sent only while install telemetry is on (`enableInstallTelemetry` or `PI_TELEMETRY=1`). Requests that pi-rust makes to its own services, such as the version check, always send the pi-rust version.

## Session sharing

`/share` uploads the current session. By default it works as in upstream pi: Radius when you are signed in to it, else a secret GitHub gist viewed on pi.dev. The first time pi runs in interactive mode, it asks where `/share` should send sessions:

- **pi.dev** (the default) sets `sessionShareDestination` to `"auto"` and `sessionShareViewer` to `"pi.dev"`.
- **pi-rust.com** sets `sessionShareDestination` to `"pi-rust"` and `sessionShareViewer` to `"pi-rust"`.

Esc keeps the pi.dev default. pi asks once: it asks while the global settings have no `sessionShareViewer`, so people who installed an earlier version are asked once too. It does not ask when `PI_CODING_AGENT_DIR` is set, or in print, JSON, and RPC modes.

`sessionShareDestination` chooses where. Set it in `settings.json`, or open `/settings` and change **Session share destination**.

| Value | Behavior |
| --- | --- |
| `"auto"` (default) | Radius when the `radius` provider has a credential, else a secret GitHub gist, as upstream pi. With `sessionShareViewer` set to `"pi-rust"`, your pi-rust.com account comes first when you are signed in. |
| `"pi-rust"` | Your pi-rust.com account. If you are not signed in, `/share` signs you in first. |
| `"radius"` | A Radius artifact. Run `/login radius` first. |
| `"gist"` | A secret GitHub gist, created with the `gh` CLI. |

A pi-rust.com share uploads the same HTML export as a gist share, up to 25 MiB, and shows `Share URL: <url>`. The share title is the session name, or `Pi session`.

To sign in, `/share` shows a code and a sign-in URL. Press Enter to open the page and copy the code, then approve the code on pi-rust.com. Esc cancels. The code expires after the time the service sets. pi saves the token in `<agent dir>/pi-rust-account.json`, readable only by you, and not in `auth.json`. If pi-rust.com rejects the token, pi deletes the file, and the next `/share` signs you in again. To sign out, run `/logout` and choose **pi-rust.com**. pi revokes the token on pi-rust.com and deletes the file.

Two settings apply to shares on your pi-rust.com account. Upstream pi has neither.

| Setting | Values | Effect |
| --- | --- | --- |
| `sessionShareExpiry` | `"never"` (default), `"1h"`, `"1d"`, `"7d"`, `"30d"` | pi-rust.com deletes the share after this time. |
| `sessionShareVisibility` | `"link"` (default), `"private"` | `"link"`: anyone with the link can open it. `"private"`: only you, signed in to pi-rust.com. |

After a share, pi shows the expiry and visibility under the URL when they are not the defaults. You can change both later, rename, download, and delete shares at [pi-rust.com/account](https://pi-rust.com/account/). Shares are not indexed by search engines.

`sessionShareViewer` chooses the page that gist links open:

| Value | Link |
| --- | --- |
| `"pi.dev"` (default) | `https://pi.dev/session/#<gist id>`, as upstream pi |
| `"pi-rust"` | `https://pi-rust.com/session/#<gist id>` |

Environment variables:

- `PI_SHARE_URL` replaces the share service origin `https://pi-rust.com` for sign-in and uploads.
- `PI_SHARE_VIEWER_URL` replaces the gist viewer base URL, whatever `sessionShareViewer` says.

## Git worktrees

In a linked git worktree (`git worktree add`), pi reads each project entry from the worktree first. When the worktree does not have it, pi reads the main checkout's copy at the same path. Thus untracked project settings, such as a `.pi/settings.json` or `.pi/mcp.json` that git ignores, apply in every worktree of the repository.

- Files: `.pi/settings.json`, `.pi/mcp.json`, `.pi/SYSTEM.md`, and `.pi/APPEND_SYSTEM.md`.
- Folders: `.pi/skills`, `.pi/prompts`, `.pi/extensions`, and `.pi/themes`. A folder in the worktree replaces the main checkout's folder of the same name.
- Relative package and resource entries, `pi config` overrides, and project package installs resolve from the folder of the `settings.json` that applies. A change to project settings in a worktree without its own file updates the main checkout's file.
- A saved trust decision for a folder in the worktree wins. Otherwise the decision for the main checkout (or its closest parent) applies, then the decisions for the parents of the worktree folder.
- Native extensions read `.pi/ext/<name>.json` with the same fallback.
