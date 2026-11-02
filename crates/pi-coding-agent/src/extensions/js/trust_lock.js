// `lockSync` of proper-lockfile 4.1.2 (MIT, LICENSE-proper-lockfile) for the vendored trust store only:
// `lib/lockfile.js`, `lib/adapter.js` and `lib/mtime-precision.js`, with `{ realpath: false, lockfilePath }`.
import { mkdirSync, rmdirSync, statSync, utimesSync } from "node:fs";
import { resolve } from "node:path";
import process from "node:process";
import { unsupportedError } from "./unsupported.js";

const STALE = 10000;
const UPDATE = 5000;
const locks = {};

function elocked(file) {
	return Object.assign(new Error("Lock file is already being held"), { code: "ELOCKED", file });
}

function probe(lockfilePath) {
	// Ceiling to the second plus 5 ms, so the mtime is "not on the second".
	const mtime = new Date(Math.ceil(Date.now() / 1000) * 1000 + 5);
	let stat;
	try {
		utimesSync(lockfilePath, mtime, mtime);
		stat = statSync(lockfilePath);
	} catch (error) {
		try {
			rmdirSync(lockfilePath);
		} catch {}
		throw error;
	}
	return { mtime: stat.mtime, mtimePrecision: stat.mtime.getTime() % 1000 === 0 ? "s" : "ms" };
}

function acquireLock(file, lockfilePath, stale) {
	try {
		mkdirSync(lockfilePath);
	} catch (error) {
		if (error.code !== "EEXIST") throw error;
		if (stale <= 0) throw elocked(file);
		let stat;
		try {
			stat = statSync(lockfilePath);
		} catch (statError) {
			if (statError.code === "ENOENT") return acquireLock(file, lockfilePath, 0);
			throw statError;
		}
		if (!(stat.mtime.getTime() < Date.now() - stale)) throw elocked(file);
		removeLock(lockfilePath);
		return acquireLock(file, lockfilePath, 0);
	}
	return probe(lockfilePath);
}

function removeLock(lockfilePath) {
	try {
		rmdirSync(lockfilePath);
	} catch (error) {
		if (error.code !== "ENOENT") throw error;
	}
}

function getMtime(precision) {
	const now = Date.now();
	return new Date(precision === "s" ? Math.ceil(now / 1000) * 1000 : now);
}

function compromise(file, lock, error) {
	lock.released = true;
	if (lock.updateTimeout) clearTimeout(lock.updateTimeout);
	if (locks[file] === lock) delete locks[file];
	throw error;
}

// The refresh timer of `lib/lockfile.js` `updateLock`: a release that finds another lock in the map
// (`ENOTACQUIRED`) leaves it running, and it throws `ECOMPROMISED` from the timer later.
function updateLock(file) {
	const lock = locks[file];
	if (lock.updateTimeout) return;
	lock.updateDelay = lock.updateDelay || UPDATE;
	lock.updateTimeout = setTimeout(() => {
		lock.updateTimeout = null;
		let stat;
		try {
			stat = statSync(lock.lockfilePath);
		} catch (error) {
			if (error.code === "ENOENT" || lock.lastUpdate + STALE < Date.now()) {
				return compromise(file, lock, Object.assign(error, { code: "ECOMPROMISED" }));
			}
			lock.updateDelay = 1000;
			return updateLock(file);
		}
		if (lock.mtime.getTime() !== stat.mtime.getTime()) {
			const error = new Error("Unable to update lock within the stale threshold");
			return compromise(file, lock, Object.assign(error, { code: "ECOMPROMISED" }));
		}
		const mtime = getMtime(lock.mtimePrecision);
		let failed = null;
		try {
			utimesSync(lock.lockfilePath, mtime, mtime);
		} catch (error) {
			failed = error;
		}
		const isOverThreshold = lock.lastUpdate + STALE < Date.now();
		if (lock.released) return;
		if (failed) {
			if (failed.code === "ENOENT" || isOverThreshold) {
				return compromise(file, lock, Object.assign(failed, { code: "ECOMPROMISED" }));
			}
			lock.updateDelay = 1000;
			return updateLock(file);
		}
		lock.mtime = mtime;
		lock.lastUpdate = Date.now();
		lock.updateDelay = null;
		updateLock(file);
	}, lock.updateDelay);
	if (lock.updateTimeout.unref) lock.updateTimeout.unref();
}

// Like `lib/lockfile.js` `unlock`: whatever lock the map holds for `file` is released, and the caller's
// `lockfilePath` is removed.
function unlock(file, lockfilePath) {
	const lock = locks[file];
	if (!lock) throw Object.assign(new Error("Lock is not acquired/owned by you"), { code: "ENOTACQUIRED" });
	if (lock.updateTimeout) clearTimeout(lock.updateTimeout);
	lock.released = true;
	delete locks[file];
	removeLock(lockfilePath);
}

let exitEmitted = false;
let exitWrapped = false;

function removeHeldLocks() {
	if (exitEmitted) return;
	exitEmitted = true;
	for (const file in locks) {
		try {
			rmdirSync(locks[file].lockfilePath);
		} catch {}
	}
}

// signal-exit 3.0.7 (ISC, LICENSE-signal-exit) `processEmit` and `processReallyExit`, installed on the
// first `lockSync`: held locks go after every `exit` listener, once. Signal listeners are left out.
function wrapExit() {
	if (exitWrapped) return;
	exitWrapped = true;
	const originalEmit = process.emit;
	const originalReallyExit = process.reallyExit;
	process.emit = function processEmit(event, ...rest) {
		if (event !== "exit") return originalEmit.call(this, event, ...rest);
		if (rest[0] !== undefined) process.exitCode = rest[0];
		const result = originalEmit.call(this, event, ...rest);
		removeHeldLocks();
		return result;
	};
	process.reallyExit = function processReallyExit(code) {
		process.exitCode = code || 0;
		removeHeldLocks();
		originalReallyExit.call(process, process.exitCode);
	};
}

function lockSync(target, options) {
	const keys = Object.keys(options ?? {}).sort().join();
	if (keys !== "lockfilePath,realpath" || options.realpath !== false || typeof options.lockfilePath !== "string") {
		throw unsupportedError("proper-lockfile lockSync without exactly { realpath: false, lockfilePath }");
	}
	wrapExit();
	const file = resolve(target);
	const lockfilePath = options.lockfilePath;
	const { mtime, mtimePrecision } = acquireLock(file, lockfilePath, STALE);
	const lock = (locks[file] = { lockfilePath, mtime, mtimePrecision, lastUpdate: Date.now() });
	updateLock(file);
	return () => {
		if (lock.released) throw Object.assign(new Error("Lock is already released"), { code: "ERELEASED" });
		unlock(file, lockfilePath);
	};
}

export default { lockSync };
