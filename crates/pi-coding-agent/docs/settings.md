# Settings

Settings load from `<agent dir>/settings.json` (normally `~/.pi/agent/settings.json`). A trusted project's `.pi/settings.json` overrides global values.

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
