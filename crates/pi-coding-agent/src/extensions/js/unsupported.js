// Exports whose upstream behavior needs host functionality pi-rust does not provide. Each one
// loads, so imports link, and throws `ERR_PI_UNSUPPORTED` naming the member when used.

export function unsupportedError(member) {
	return Object.assign(new Error(`${member} is not supported in pi-rust`), { code: "ERR_PI_UNSUPPORTED" });
}

export function unsupportedFunction(member) {
	return () => {
		throw unsupportedError(member);
	};
}

export function unsupportedClass(member) {
	return class {
		constructor() {
			throw unsupportedError(member);
		}
	};
}
