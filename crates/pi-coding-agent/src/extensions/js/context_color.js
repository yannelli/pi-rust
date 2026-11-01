// `chalk.yellow` for the vendored context loader: chalk 6.0.0 (MIT, see LICENSE-chalk) with its vendored
// supports-color, limited to the default instance's stdout level and the `yellow` builder.
import process from "node:process";
import os from "node:os";
import tty from "node:tty";

function hasFlag(flag, argv = globalThis.Deno ? globalThis.Deno.args : process.argv) {
	const prefix = flag.startsWith("-") ? "" : flag.length === 1 ? "-" : "--";
	const position = argv.indexOf(prefix + flag);
	const terminatorPosition = argv.indexOf("--");
	return position !== -1 && (terminatorPosition === -1 || position < terminatorPosition);
}

const { env } = process;

let flagForceColor;
if (hasFlag("no-color") || hasFlag("no-colors") || hasFlag("color=false") || hasFlag("color=never")) {
	flagForceColor = 0;
} else if (hasFlag("color") || hasFlag("colors") || hasFlag("color=true") || hasFlag("color=always")) {
	flagForceColor = 1;
}

function hasNumericForceColor() {
	return /^\d+$/.test(env.FORCE_COLOR);
}

function envForceColor() {
	if (!("FORCE_COLOR" in env)) {
		return;
	}
	if (env.FORCE_COLOR === "false") {
		return 0;
	}
	if (env.FORCE_COLOR === "true" || env.FORCE_COLOR.length === 0) {
		return 1;
	}
	if (!hasNumericForceColor()) {
		return;
	}
	return Math.min(Number.parseInt(env.FORCE_COLOR, 10), 3);
}

function translateLevel(level) {
	if (level === 0) {
		return false;
	}
	return { level, hasBasic: true, has256: level >= 2, has16m: level >= 3 };
}

function _supportsColor(haveStream, { streamIsTTY, sniffFlags = true } = {}) {
	const noFlagForceColor = envForceColor();
	if (noFlagForceColor !== undefined) {
		flagForceColor = noFlagForceColor;
	}
	const forceColor = sniffFlags ? flagForceColor : noFlagForceColor;
	if (forceColor === 0) {
		return 0;
	}
	if (sniffFlags) {
		if (hasFlag("color=16m") || hasFlag("color=full") || hasFlag("color=truecolor")) {
			return 3;
		}
		if (hasFlag("color=256")) {
			return 2;
		}
	}
	if (forceColor !== undefined && hasNumericForceColor()) {
		return forceColor;
	}
	if ("TF_BUILD" in env && "AGENT_NAME" in env) {
		return 1;
	}
	if (haveStream && !streamIsTTY && forceColor === undefined) {
		return 0;
	}
	const min = forceColor || 0;
	if (env.TERM === "dumb") {
		return min;
	}
	if (process.platform === "win32") {
		const osRelease = os.release().split(".");
		if (Number(osRelease[0]) >= 10 && Number(osRelease[2]) >= 10_586) {
			return Number(osRelease[2]) >= 14_931 ? 3 : 2;
		}
		return 1;
	}
	if ("CI" in env) {
		if (["GITHUB_ACTIONS", "GITEA_ACTIONS", "CIRCLECI"].some((key) => key in env)) {
			return 3;
		}
		if (["TRAVIS", "APPVEYOR", "GITLAB_CI", "BUILDKITE", "DRONE"].some((sign) => sign in env) || env.CI_NAME === "codeship") {
			return 1;
		}
		return min;
	}
	if ("TEAMCITY_VERSION" in env) {
		return /^(?:9\.0*[1-9]\d*\.|\d{2,}\.)/.test(env.TEAMCITY_VERSION) ? 1 : 0;
	}
	if (env.COLORTERM === "truecolor") {
		return 3;
	}
	if (env.TERM === "xterm-kitty" || env.TERM === "xterm-ghostty" || env.TERM === "wezterm") {
		return 3;
	}
	if ("TERM_PROGRAM" in env) {
		const version = Number.parseInt((env.TERM_PROGRAM_VERSION || "").split(".", 1)[0], 10);
		switch (env.TERM_PROGRAM) {
			case "iTerm.app": {
				return version >= 3 ? 3 : 2;
			}
			case "Apple_Terminal": {
				return 2;
			}
		}
	}
	if (/-256(?:color)?$/i.test(env.TERM)) {
		return 2;
	}
	if (/^screen|^xterm|^vt100|^vt220|^rxvt|color|ansi|cygwin|linux/i.test(env.TERM)) {
		return 1;
	}
	if ("COLORTERM" in env) {
		return 1;
	}
	return min;
}

function createSupportsColor(stream, options = {}) {
	return translateLevel(_supportsColor(stream, { streamIsTTY: stream && stream.isTTY, ...options }));
}

const stdoutColor = createSupportsColor({ isTTY: tty.isatty(1) });
const LEVEL = stdoutColor ? stdoutColor.level : 0;
const OPEN = "\u001B[33m";
const CLOSE = "\u001B[39m";

function stringReplaceAll(string, substring, postfix) {
	let index = string.indexOf(substring);
	if (index === -1) {
		return string;
	}
	const substringLength = substring.length;
	let endIndex = 0;
	let returnValue = "";
	do {
		returnValue += string.slice(endIndex, index) + substring + postfix;
		endIndex = index + substringLength;
		index = string.indexOf(substring, endIndex);
	} while (index !== -1);
	returnValue += string.slice(endIndex);
	return returnValue;
}

function stringEncaseCRLFWithFirstIndex(string, prefix, postfix, index) {
	let endIndex = 0;
	let returnValue = "";
	do {
		const isGotCR = string[index - 1] === "\r";
		returnValue += string.slice(endIndex, isGotCR ? index - 1 : index) + prefix + (isGotCR ? "\r\n" : "\n") + postfix;
		endIndex = index + 1;
		index = string.indexOf("\n", endIndex);
	} while (index !== -1);
	returnValue += string.slice(endIndex);
	return returnValue;
}

function applyStyle(string) {
	if (LEVEL <= 0 || !string) {
		return string;
	}
	if (string.includes("\u001B")) {
		string = stringReplaceAll(string, CLOSE, OPEN);
	}
	const lfIndex = string.indexOf("\n");
	if (lfIndex !== -1) {
		string = stringEncaseCRLFWithFirstIndex(string, CLOSE, OPEN, lfIndex);
	}
	return OPEN + string + CLOSE;
}

function yellow(...arguments_) {
	if (arguments_.length === 1) {
		return applyStyle("" + arguments_[0]);
	}
	if (arguments_.length === 2) {
		return applyStyle(arguments_[0] + " " + arguments_[1]);
	}
	return applyStyle(arguments_.join(" "));
}

export default Object.freeze({ yellow });
