# Terminal

## Themes

To select a harness theme, open `/settings` and select Theme. The themes are `system`, `dark`, `light`, `ghostty`, `darcula-islands`, `nord`, and `catppuccin`.

The added dark palettes are based on the Ghostty default colors, JetBrains Darcula and Islands, Nord, and Catppuccin Mocha.

## Notifications

In interactive mode, when a run takes 10 seconds or more, pi sends a terminal notification at the end. Its title is "pi". The text is "Done" or the error.

| Terminal | Sequence |
| --- | --- |
| Ghostty, WezTerm, foot, and rxvt | OSC 777 |
| kitty | OSC 99 |
| iTerm2 and Windows Terminal | OSC 9 |
| Other terminals, for example tmux and screen | BEL |

In fullscreen mode, pi does not send the notification while the terminal reports focus.

Settings:

- `terminal.notifyOnFinish`: `"auto"` (the default), `"osc"`, `"bell"`, or `"off"`.
- `terminal.notifyMinSeconds`: the minimum run time for a notification, in seconds. The default is 10.

## iTerm2 integration

The iTerm2 integration is the `marketplace:iterm2` extension. It sets the tab color from the agent state, shows a badge with the session name, sets user variables, requests attention when a run finishes, and adds `/terminal-setup`. Earlier pi-rust versions had it built in.

To install it:

```bash
pi install marketplace:iterm2
```

The extension reads `terminal.iterm2` in `~/.pi/agent/settings.json`, so an existing setting continues to work. See the extension's documentation in [yannelli/pi-rust-extensions](https://github.com/yannelli/pi-rust-extensions).

When `terminal.iterm2` is on and the extension is not installed, interactive mode asks one time at startup to install it, with the choices **Install**, **Not now**, and **Do not ask again**. Without the extension, `/terminal-setup` shows the install command. iTerm2 inline images (`terminal.images`) are part of pi and need no extension.
