# Status line

The custom status line is the `marketplace:statusline` extension. Earlier pi-rust versions had it built in.

To install it:

```bash
pi install marketplace:statusline
```

You can also install it from `/extensions`. The extension adds `/statusline` and reads the `statusLine` setting in `~/.pi/agent/settings.json`, so an existing setting continues to work. See the extension's documentation in [yannelli/pi-rust-extensions](https://github.com/yannelli/pi-rust-extensions).

Without the extension, pi shows the upstream footer: the current working directory, the token counts, the context use, and the model. Lines from extension `setStatus` calls show below it. An extension `setFooter` replaces the whole footer.

## Move from the built-in status line

When `statusLine` is set and the extension is not installed, interactive mode asks one time at startup:

> The status line is now an extension. Install marketplace:statusline?

- **Install** installs the extension, adds `marketplace:statusline` to `packages`, and reloads the extensions.
- **Not now** asks again at the next start.
- **Do not ask again** saves the choice in `~/.pi/agent/marketplace/migrations.json`. pi does not change `settings.json`.

If pi cannot get the extension, for example offline, it shows the `pi install marketplace:statusline` command. Without the extension, `/statusline` shows the same command. `pi doctor` shows a warning when `statusLine` is set and the extension is not installed.
