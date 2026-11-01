// Drives the exported `TuiAltScreen` through a fake terminal's input callback, on upstream under Node
// (expected output) and in the extension host. Only plain values are recorded.
const PAGE_UP = "\x1b[5~";
const PAGE_DOWN = "\x1b[6~";
const CTRL_HOME = "\x1b[1;5H";
const WHEEL_UP_ON_FIRST_ROW = "\x1b[<64;2;1M";

const plain = (text) => text;
const selectTheme = {
	selectedPrefix: plain,
	selectedText: plain,
	description: plain,
	scrollInfo: plain,
	noMatch: plain,
};
const lines = (count) => Array.from({ length: count }, (_, index) => `line ${index + 1}`).join("\n");
async function autocompleteReady(editor) {
	const deadline = Date.now() + 1000;
	while (!editor.isShowingAutocomplete()) {
		if (Date.now() >= deadline) throw new Error("Autocomplete did not open");
		await new Promise((resolve) => setTimeout(resolve, 0));
	}
}

/** A terminal with every `Terminal` member; `send` delivers input as a real terminal's `onInput` does. */
function fakeTerminal(columns, rows) {
	let onInput;
	return {
		send(data) {
			onInput?.(data);
		},
		start(input) {
			onInput = input;
		},
		stop() {
			onInput = undefined;
		},
		drainInput: async () => {},
		write() {},
		get columns() {
			return columns;
		},
		get rows() {
			return rows;
		},
		get kittyProtocolActive() {
			return false;
		},
		moveBy() {},
		hideCursor() {},
		showCursor() {},
		clearLine() {},
		clearFromCursor() {},
		clearScreen() {},
		setTitle() {},
		setProgress() {},
		setProgramStatus() {},
	};
}

/** Start a fullscreen TUI on a fake terminal, run `steps`, and stop it whatever happens. */
async function fullscreen(t, columns, rows, steps) {
	const terminal = fakeTerminal(columns, rows);
	const tui = new t.TuiAltScreen(terminal);
	try {
		return await steps(tui, (data) => {
			terminal.send(data);
			tui.renderNow();
		});
	} finally {
		tui.stop();
	}
}

function twelveItems(t) {
	const items = Array.from({ length: 12 }, (_, index) => ({ value: `item-${index}`, label: `item-${index}` }));
	return new t.SelectList(items, 3, selectTheme);
}

export async function scenario(t) {
	const list = await fullscreen(t, 20, 6, async (tui, send) => {
		const list = twelveItems(t);
		tui.addChild(new t.Text(lines(12), 0, 0));
		tui.addChild(list);
		tui.setFocus(list);
		tui.start();
		tui.renderNow();
		const top = tui.viewportTop;
		send(PAGE_DOWN);
		send(CTRL_HOME);
		const selected = list.getSelectedItem()?.value ?? null;
		return { capture: list.capturesViewportInput ?? null, selected, topKept: tui.viewportTop === top };
	});

	const flip = await fullscreen(t, 20, 6, async (tui, send) => {
		const seen = [];
		const box = {
			capture: true,
			get capturesViewportInput() {
				return this.capture;
			},
			render: () => ["box"],
			handleInput: (data) => seen.push(data),
			invalidate() {},
		};
		tui.addChild(new t.Text(lines(30), 0, 0));
		tui.addChild(box);
		tui.setFocus(box);
		tui.start();
		tui.renderNow();
		const top = tui.viewportTop;
		send(CTRL_HOME);
		const deferred = { deferredSeen: seen.length, deferredTopKept: tui.viewportTop === top };
		box.capture = false;
		send(PAGE_UP);
		return { ...deferred, transcriptScrolled: tui.viewportTop < top, seenAfter: seen.length };
	});

	const wheel = await fullscreen(t, 20, 6, async (tui, send) => {
		const list = twelveItems(t);
		tui.addChild(new t.Text(lines(12), 0, 0));
		tui.addChild(list);
		tui.setFocus(list);
		tui.start();
		tui.renderNow();
		const top = tui.viewportTop;
		send(WHEEL_UP_ON_FIRST_ROW);
		return { wheelScrolled: tui.viewportTop < top };
	});

	const editor = await fullscreen(t, 40, 8, async (tui, send) => {
		const editor = new t.Editor(tui, { borderColor: plain, selectList: selectTheme });
		let submitted = null;
		editor.onSubmit = (text) => {
			submitted = text;
		};
		editor.setAutocompleteProvider({
			getSuggestions: async () => ({
				items: Array.from({ length: 12 }, (_, index) => ({ value: `/command-${index}`, label: `command-${index}` })),
				prefix: "/",
			}),
			applyCompletion: (lines, cursorLine, _cursorCol, item) => ({
				lines: lines.map((line, index) => (index === cursorLine ? item.value : line)),
				cursorLine,
				cursorCol: item.value.length,
			}),
		});
		tui.addChild(new t.Text(lines(30), 0, 0));
		tui.addChild(editor);
		tui.setFocus(editor);
		tui.start();
		tui.renderNow();
		send("/");
		await autocompleteReady(editor);
		tui.renderNow();
		const menuOpen = editor.isShowingAutocomplete();
		const withMenu = tui.viewportTop;
		send(PAGE_DOWN);
		const pagedTopKept = tui.viewportTop === withMenu;
		send("\r");
		const menuClosed = !editor.isShowingAutocomplete();
		const withoutMenu = tui.viewportTop;
		send(PAGE_UP);
		return { menuOpen, pagedTopKept, submitted, menuClosed, transcriptScrolled: tui.viewportTop < withoutMenu };
	});

	return { list, flip, wheel, editor };
}
