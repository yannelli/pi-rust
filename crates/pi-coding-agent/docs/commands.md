# Interactive commands

In interactive mode, type `/` to see the list of commands. Examples are `/login`, `/model`, `/settings`, `/tree`, `/fork`, `/compact`, `/export`, `/share`, `/mcp`, and `/hotkeys`.

If you type while the agent works, pi puts your input in the queue as steering. To put it in the queue as a follow-up, press Alt+Enter. On Windows, press Ctrl+Q.

pi-rust adds `/diff`, `/context`, `/init`, `/review`, `/undo`, `/retry`, and the prompt history. `/export` is also in upstream pi. pi-rust adds its Markdown output.

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
