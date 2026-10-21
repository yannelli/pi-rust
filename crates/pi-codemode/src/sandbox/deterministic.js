(function (now, random, startedAt) {
	"use strict";

	// Runs before the prelude freezes the built-ins. Replaces what depends on the host: the clock (virtual
	// time), Math.random (a seeded stream), the timezone (UTC), and WeakRef/FinalizationRegistry (removed).
	const D = Date;
	const P = D.prototype;
	const def = (object, key, value) =>
		Object.defineProperty(object, key, { value, writable: true, enumerable: false, configurable: true });
	const withLength = (fn, length) => Object.defineProperty(fn, "length", { value: length, configurable: true });

	delete globalThis.WeakRef;
	delete globalThis.FinalizationRegistry;

	def(Math, "random", { random: () => random() }.random);

	const time = () => Math.round(now());
	Object.defineProperty(globalThis, "performance", {
		value: Object.freeze({ now: () => now() - startedAt, timeOrigin: startedAt }),
		writable: true,
		enumerable: true,
		configurable: true,
	});

	const getTime = P.getTime;
	const hostOffset = P.getTimezoneOffset;
	const pad = (value, width) => String(value).padStart(width, "0");
	const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
	const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
	const year = (value) => (value < 0 ? "-" + pad(-value, 4) : pad(value, 4));

	// The local-time methods read and write UTC.
	for (const part of ["FullYear", "Month", "Date", "Day", "Hours", "Minutes", "Seconds", "Milliseconds"]) {
		const read = P["getUTC" + part];
		const name = "get" + part;
		def(P, name, { [name]() { return read.call(this); } }[name]);
	}
	for (const part of ["FullYear", "Month", "Date", "Hours", "Minutes", "Seconds", "Milliseconds"]) {
		const write = P["setUTC" + part];
		const name = "set" + part;
		def(P, name, withLength({ [name](...args) { return Reflect.apply(write, this, args); } }[name], write.length));
	}
	const readYear = P.getUTCFullYear;
	const writeYear = P.setUTCFullYear;
	def(P, "getYear", { getYear() { return readYear.call(this) - 1900; } }.getYear);
	def(P, "setYear", withLength({ setYear(value) {
		const full = Number(value);
		const whole = Math.trunc(full);
		return writeYear.call(this, whole >= 0 && whole <= 99 ? whole + 1900 : full);
	} }.setYear, 1));
	def(P, "getTimezoneOffset", { getTimezoneOffset() {
		const value = getTime.call(this);
		return value === value ? 0 : NaN;
	} }.getTimezoneOffset);

	// The engine's strings for local time, with the zone fixed to GMT+0000.
	const readMonth = P.getUTCMonth;
	const readDay = P.getUTCDate;
	const readWeekday = P.getUTCDay;
	const readHours = P.getUTCHours;
	const readMinutes = P.getUTCMinutes;
	const readSeconds = P.getUTCSeconds;
	function format(date, kind) {
		const value = getTime.call(date);
		if (value !== value) return "Invalid Date";
		const y = year(readYear.call(date));
		const month = readMonth.call(date);
		const day = pad(readDay.call(date), 2);
		const h = readHours.call(date);
		const clock = pad(h, 2) + ":" + pad(readMinutes.call(date), 2) + ":" + pad(readSeconds.call(date), 2);
		const dated = DAYS[readWeekday.call(date)] + " " + MONTHS[month] + " " + day + " " + y;
		const slashes = pad(month + 1, 2) + "/" + day + "/" + y;
		const twelve = pad((h + 11) % 12 + 1, 2) + clock.slice(2) + (h < 12 ? " AM" : " PM");
		switch (kind) {
			case "toString": return dated + " " + clock + " GMT+0000";
			case "toDateString": return dated;
			case "toTimeString": return clock + " GMT+0000";
			case "toLocaleString": return slashes + ", " + twelve;
			case "toLocaleDateString": return slashes;
			default: return twelve;
		}
	}
	for (const name of ["toString", "toDateString", "toTimeString", "toLocaleString", "toLocaleDateString", "toLocaleTimeString"]) {
		def(P, name, { [name]() { return format(this, name); } }[name]);
	}

	// The engine reads a string without a zone in the host's timezone. Read it as UTC instead.
	const parseHost = D.parse;
	const DATE_ONLY = /^[+-]?\d{4,6}(?:-\d\d(?:-\d\d)?)?$/;
	const ZONE = /(?:\d|\s)Z$|\b(?:GMT|UTC?|[ECMP][SD]T)\b|\d\s?[+-]\d\d(?::?\d\d)?(?:\s*\(.*\))?$/i;
	function parse(value) {
		const text = String(value);
		const parsed = parseHost.call(D, text);
		if (parsed !== parsed || DATE_ONLY.test(text.trim()) || ZONE.test(text)) return parsed;
		return parsed - hostOffset.call(new D(parsed)) * 60000;
	}

	const Replacement = function Date(...args) {
		if (new.target === undefined) return format(new D(time()), "toString");
		let value;
		if (args.length === 0) value = time();
		else if (args.length === 1) {
			if (typeof args[0] !== "string") return Reflect.construct(D, args, new.target);
			value = parse(args[0]);
		} else value = D.UTC(...args);
		return Reflect.construct(D, [value], new.target);
	};
	withLength(Replacement, 7);
	def(Replacement, "now", { now: () => time() }.now);
	def(Replacement, "parse", withLength({ parse(value) { return parse(value); } }.parse, 1));
	def(Replacement, "UTC", D.UTC);
	Object.defineProperty(Replacement, "prototype", { value: P, writable: false, enumerable: false, configurable: false });
	def(P, "constructor", Replacement);
	def(globalThis, "Date", Replacement);
})
