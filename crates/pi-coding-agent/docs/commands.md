# Interactive commands

In interactive mode, type `/` to see the list of commands. Examples are `/login`, `/model`, `/settings`, `/tree`, `/fork`, `/compact`, `/export`, `/share`, `/mcp`, and `/hotkeys`.

If you type while the agent works, pi puts your input in the queue as steering. To put it in the queue as a follow-up, press Alt+Enter. On Windows, press Ctrl+Q.

pi-rust adds `/diff`, `/context`, `/init`, `/review`, `/undo`, `/retry`, `/extensions`, `/widgets`, and the prompt history. `/export` is also in upstream pi. pi-rust adds its Markdown output.

## /diff

`/diff [--stat] [path]` shows the uncommitted git changes in the working directory:

- The `git diff HEAD` stat.
- The untracked files.
- The diff of the staged and unstaged changes.

With `--stat`, `/diff` does not show the diff. With a path, it shows only the changes to that path. It shows a maximum of 500 lines. A note gives the number of lines that it did not show.

## /context

`/context` shows how much of the context window of the current model the conversation uses. It shows these items:

- The model and the window size.
- The tokens in use and their percentage.
- The point where auto-compaction starts.
- A table with the estimated tokens for each part of the context, and the free space.

The parts of the context are the system prompt, tool definitions, context files, skills, user messages, assistant messages, and tool results.

The count of tokens in use is the usage that the last response reported, plus estimates for newer messages. Before a response reports usage, the count is an estimate of the next request.

## /export

`/export [file]` writes the session to an HTML file. If the file name ends in `.jsonl`, it writes JSONL. `pi --export <session.jsonl> [output]` exports a saved session file to HTML.

With both commands, an output path that ends in `.md` gives a Markdown file. The Markdown file contains the current branch:

- A title, the date, the models, the working directory, the tokens, and the cost.
- Each user turn and assistant turn.
- Thinking in collapsed `<details>` blocks.
- Tool calls with their JSON arguments.
- Tool results. Each result stops after 200 lines.

Images change to placeholders.

## /init

`/init [instructions]` tells the agent to examine the project in the working directory. Then the agent writes an `AGENTS.md` file there, based only on what it finds. The file contains these items:

- An overview.
- The build, test, and lint commands.
- The code layout.
- The conventions.
- The known problems.

If the directory already has a context file that pi loads, the agent improves that file. These context files are `AGENTS.md`, `CLAUDE.md`, and `AGENTS.override.md`.

pi adds the text after `/init` to the prompt as more instructions. The prompt is a normal message. It shows in the chat, uses the current model, and pi saves it in the session. As with other built-in commands, `/init` has priority over a prompt template or an extension command with the name `init`. `/init` works only in interactive mode.

## /review

`/review [instructions]` tells the agent to review the uncommitted changes in the working directory. The prompt tells the agent to do these steps:

1. Run `git status` and `git diff HEAD`.
2. Read the untracked files.
3. Report bugs, risky changes, and missing tests in order of severity, with `path:line` references.

The agent does not change files. pi adds the text after `/review` as more instructions, for example an area to examine or a base branch for the comparison. Outside a git repository, `/review` shows an error and sends nothing. As with `/init`, the prompt is a normal message.

## /undo

`/undo` restores the files that the edit and write tools changed during the last prompt. It writes back the earlier content of these files and deletes the files that the agent created. To go back one more prompt, run `/undo` again.

If a file changed after the agent wrote it, `/undo` does not restore that file and shows it in a list. For example, you or a bash command can change the file. `/undo` does not change the conversation. To move in the conversation, use `/tree`.

`/undo` has these limits:

- It records only the changes from the edit and write tools. It does not record the changes from bash commands.
- It keeps checkpoints in memory for the last 20 prompts of the current session.
- It does not keep files that are larger than 4 MB.
- It loses all checkpoints when pi stops or when you change to a different session.

## /retry

`/retry` runs the last prompt of the current branch again:

1. It moves to the point before that prompt, as `/tree` does.
2. It sends the same text and images as a new prompt.

The new answer starts a new branch. To go back to the old answer, use `/tree`.

`/retry` ignores custom messages and prompts from extensions, so it uses the last prompt that you sent. pi knows which prompts came from extensions only while the session that received them is open. After a resume, pi does not know. If the agent works or compacts, or if there is no prompt yet, `/retry` shows an error and sends nothing.

## /extensions

`/extensions` opens a browser with two tabs:

- **Installed** shows the packages in the `packages` settings first, then the extensions that pi loads. The details of a package show its extensions. A filled dot (●) shows an enabled extension. An empty dot (○) shows a disabled extension.
- **Marketplace** shows the extensions in the latest signed marketplace index. A check mark (✓) shows an installed extension. The [Marketplace](marketplace.md) page gives the index and verification rules.

A row can show the version, the source, `disabled`, and `update available`.

| Key | Action |
| --- | --- |
| Tab, Shift+Tab | Go to the other tab. |
| Typed text | Search the names, IDs, descriptions, sources, and tags. |
| Up, Down, Page Up, Page Down | Select a row. |
| Enter | Show the details and the actions of the selected row. In the details, use Up and Down to select an action, and Enter to run it. |
| Space | On the Installed tab, enable or disable the selected extension. For a package, this applies to all of its extensions. When the search box contains text, Space types a space. |
| Ctrl+I | On the Marketplace tab, install the selected extension. |
| Ctrl+U | Update the selected package. On the Marketplace tab, this applies to an installed extension. A row with `update available` has a newer release. |
| Ctrl+D | Remove the selected package, or the selected installed extension on the Marketplace tab. Press `y` to remove it, or `n` or Esc to cancel. |
| Ctrl+R | Close the browser and reload, as `/reload` does. This key works after a change. |
| Esc | Close the details or clear the search. With an empty search, close the browser. |

Terminals without the Kitty keyboard protocol send Ctrl+I as Tab. In these terminals, press Enter and select **Install**.

Changes:

- Install adds `marketplace:<name>` to the `packages` user settings. The [Marketplace](marketplace.md) page gives the files that pi writes.
- Remove deletes the package and removes its source from the settings.
- Enable and disable write the same extension entries in the settings as `pi config`.
- Update moves a marketplace extension to the latest release. A source with a tag stays on its release. Other packages update as `pi update <source>` does.

After a change, the browser shows `Extensions changed. Ctrl+R to reload.` pi loads the change after the reload. While a change runs, the browser shows `busy: changes disabled`. Until the change is complete, you cannot start a different change or close the browser.

If the marketplace index does not load, the Marketplace tab shows `Marketplace unavailable:` and the error. For example, the signature is not correct, or `PI_OFFLINE=1` is set. The Installed tab continues to work. If an install fails, the status line shows `Extension change failed:` and the error. For example, the SHA-256 digest of the archive is not correct. pi does not install the extension or change the settings.

## /widgets

Extensions show widgets above and below the editor, for example the todo list, background tasks, subagents, and goals. All widgets together can use a third of the terminal rows. When they need more rows, each widget shows only its first line and the number of hidden lines. When there are more widgets than rows in the budget, the last row shows `+N more widgets · /widgets`.

- `/widgets` shows all widgets in full in the chat, each under its key.
- `/widgets collapse` shows one line for each widget, `/widgets expand` shows all lines, and `/widgets auto` collapses the widgets only when they need more rows than the budget.
- Alt+W (`app.widgets.toggle`) changes between the collapsed and the expanded view. On macOS, Option+W sends Alt+W only when the terminal uses Option as Meta.

## /statusline and /terminal-setup

These commands are in marketplace extensions:

| Command | Extension | Install |
| --- | --- | --- |
| `/statusline` | `marketplace:statusline` | `pi install marketplace:statusline` |
| `/terminal-setup` | `marketplace:iterm2` | `pi install marketplace:iterm2` |

Without the extension, the command shows the install command. See [Status line](status-line.md) and [iTerm2 integration](terminal.md#iterm2-integration).

## Prompt history

The interactive editor keeps your prompts from all sessions and projects in `~/.pi/agent/prompt-history.jsonl`. The file has one JSON string on each line. On Unix, only you can read the file. If a prompt starts with a space, pi does not save it in the file.

To move through the prompts, press Up and Down. The newest prompt comes first.

To search the history:

1. Press Ctrl+R to start a reverse search.
2. Type text to filter the prompts. The search ignores case.
3. To go to the next older match, press Ctrl+R again.
4. To put the match in the editor, press Enter. pi does not send it.

To go back to your draft, press Esc.

Settings:

- To turn off the history, set `"promptHistory": {"enabled": false}` in `settings.json`.
- To keep a different number of prompts, set `"maxEntries"`. The default is 1000.
- To use a different key for the search, set `tui.editor.historySearch` in `keybindings.json`.
