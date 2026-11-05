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

This applies to command and extension selection menus, settings, config resources, sessions, the session tree, user messages, model and provider selectors, and the extensions browser. Native extensions can read the same key with `host.settings::<String>("listOverflow")` and fit their own lists.

## Git worktrees

In a linked git worktree (`git worktree add`), pi reads each project entry from the worktree first. When the worktree does not have it, pi reads the main checkout's copy at the same path. Thus untracked project settings, such as a `.pi/settings.json` or `.pi/mcp.json` that git ignores, apply in every worktree of the repository.

- Files: `.pi/settings.json`, `.pi/mcp.json`, `.pi/SYSTEM.md`, and `.pi/APPEND_SYSTEM.md`.
- Folders: `.pi/skills`, `.pi/prompts`, `.pi/extensions`, and `.pi/themes`. A folder in the worktree replaces the main checkout's folder of the same name.
- Relative package and resource entries, `pi config` overrides, and project package installs resolve from the folder of the `settings.json` that applies. A change to project settings in a worktree without its own file updates the main checkout's file.
- A saved trust decision for a folder in the worktree wins. Otherwise the decision for the main checkout (or its closest parent) applies, then the decisions for the parents of the worktree folder.
- Native extensions read `.pi/ext/<name>.json` with the same fallback.
