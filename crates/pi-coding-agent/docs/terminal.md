# Terminal

## Themes

To select a harness theme, open `/settings` and select Theme. The themes are `system`, `dark`, `light`, `ghostty`, `darcula-islands`, `nord`, and `catppuccin`.

The added dark palettes are based on the Ghostty default colors, JetBrains Darcula and Islands, Nord, and Catppuccin Mocha. The status line has its own palettes with the same names. See [Status line](status-line.md).

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

The iTerm2 integration is off by default. To turn it on, run `/terminal-setup`, or set `terminal.iterm2` in `settings.json`:

```json
{ "terminal": { "iterm2": true } }
```

When pi runs in iTerm2, the integration does these things:

| Feature | Key | What it does |
| --- | --- | --- |
| Tab color | `tabColor` | Light blue while pi works, amber while a dialog waits for you, green when pi is done, and rose after an error. Black titles stay readable on these colors, also when iTerm2 dims inactive tabs. iTerm2 sets the title text color, and pi does not change profile text colors. When the session is idle or pi exits, pi restores the tab color of the profile. |
| Badge | `badge` | The badge shows `π` and the session name. If the session has no name, the badge shows the folder name. pi removes the badge when it exits. |
| User variables | `userVars` | pi sets `user.pi_state` (`idle`, `working`, `blocked`, `done`, or `error`), `user.pi_session`, `user.pi_project`, and `user.pi_model`. Show them in a status bar component, a badge, or a tab title, for example `\(user.pi_state)`. |
| Attention | `attention` | When a run takes `terminal.notifyMinSeconds` or more (10 by default), and pi does not know that the terminal has focus, iTerm2 bounces the Dock icon. |
| Current directory | `cwd` | iTerm2 gets the current working directory of the session, so a new tab can open in it. |

`true` turns on all features. In an object, a feature that you do not name is on. For example, `{ "badge": false }` turns on all features except the badge.

pi finds iTerm2 through `TERM_PROGRAM`, `LC_TERMINAL`, or `ITERM_SESSION_ID`. Thus the integration also works in tmux and over SSH. In tmux, run `tmux set -g allow-passthrough on` so that the sequences get to iTerm2.

### /terminal-setup

`/terminal-setup` works as in Claude Code. In iTerm2, it does these steps:

1. It sets `terminal.iterm2` to `true` in `~/.pi/agent/settings.json` if the value is unset or `false`. It keeps an existing object that selects features.
2. On the local Mac, it lets programs write to the clipboard, so `/copy` works. It does not do this over SSH. The command is `defaults write com.googlecode.iterm2 AllowClipboardAccess -bool true`. Restart iTerm2 to apply this change.
3. If [tmc/it2](https://github.com/tmc/it2) is installed, it offers to set Option-as-Alt and escape-sequence notification alerts for the current pane. Before you accept, run `it2 auth check` and approve API access. pi asks before it changes the pane, and it does not change saved profiles.
4. If it cannot do a step, it shows the manual steps. It also shows how to add a status bar component that shows `user.pi_state`.

pi does not install external utilities. In other terminals, `/terminal-setup` changes nothing.
