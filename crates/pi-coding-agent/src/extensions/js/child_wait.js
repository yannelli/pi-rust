// Waiting for a child process without hanging on inherited stdio handles (upstream
// `waitForChildProcess` in utils/child-process.ts, issue 5303).

const EXIT_STDIO_GRACE_MS = 100;

// Resolve with the exit code once the child exited and its pipes ended or fell idle. The grace
// restarts on each chunk after `exit`, so a writing descendant is read to the end.
export function waitForChildProcess(child) {
	return new Promise((resolve, reject) => {
		let settled = false;
		let exited = false;
		let exitCode = null;
		let postExitTimer;
		let stdoutEnded = child.stdout === null;
		let stderrEnded = child.stderr === null;

		const cleanup = () => {
			clearTimeout(postExitTimer);
			postExitTimer = undefined;
			child.removeListener("error", onError);
			child.removeListener("exit", onExit);
			child.removeListener("close", onClose);
			child.stdout?.removeListener("end", onStdoutEnd);
			child.stderr?.removeListener("end", onStderrEnd);
			child.stdout?.removeListener("data", onData);
			child.stderr?.removeListener("data", onData);
		};
		const finalize = (code) => {
			if (settled) return;
			settled = true;
			cleanup();
			child.stdout?.destroy();
			child.stderr?.destroy();
			resolve(code);
		};
		const maybeFinalizeAfterExit = () => {
			if (exited && !settled && stdoutEnded && stderrEnded) finalize(exitCode);
		};
		const armIdleTimer = () => {
			clearTimeout(postExitTimer);
			postExitTimer = setTimeout(() => finalize(exitCode), EXIT_STDIO_GRACE_MS);
		};
		const onData = () => {
			if (exited && !settled) armIdleTimer();
		};
		const onStdoutEnd = () => {
			stdoutEnded = true;
			maybeFinalizeAfterExit();
		};
		const onStderrEnd = () => {
			stderrEnded = true;
			maybeFinalizeAfterExit();
		};
		const onError = (error) => {
			if (settled) return;
			settled = true;
			cleanup();
			reject(error);
		};
		const onExit = (code) => {
			exited = true;
			exitCode = code;
			maybeFinalizeAfterExit();
			if (!settled) armIdleTimer();
		};
		const onClose = (code) => finalize(code);

		child.stdout?.once("end", onStdoutEnd);
		child.stderr?.once("end", onStderrEnd);
		child.stdout?.on("data", onData);
		child.stderr?.on("data", onData);
		child.once("error", onError);
		child.once("exit", onExit);
		child.once("close", onClose);
	});
}
