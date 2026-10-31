// Drives the compat faux provider and image API of `@earendil-works/pi-ai`, on upstream under Node
// (expected output) and in the extension host (`sdk_utilities.rs`).
const ID = /^[a-z]+:\d{13}:[a-z0-9]+$/;

/** Times and generated ids differ per run; everything else must match. */
function stable(value) {
	if (Array.isArray(value)) return value.map(stable);
	if (value && typeof value === "object") {
		return Object.fromEntries(
			Object.entries(value).map(([key, entry]) => {
				if (key === "durationMs") {
					if (typeof entry !== "number" || !Number.isFinite(entry) || entry < 0) {
						throw new Error("durationMs must be a finite nonnegative number");
					}
					return [key, "<duration>"];
				}
				return [key, key === "timestamp" ? "<time>" : stable(entry)];
			}),
		);
	}
	return typeof value === "string" && ID.test(value) ? "<id>" : value;
}

const error = async (run) => {
	try {
		return await run();
	} catch (failure) {
		return `${failure.constructor.name}: ${failure.message}`;
	}
};

export async function scenario(ai) {
	const faux = ai.registerFauxProvider({
		api: "faux-compat",
		provider: "faux-compat",
		models: [{ id: "m1", name: "Model One" }],
		tokenSize: { min: 1000, max: 1000 },
	});
	faux.setResponses([
		ai.fauxAssistantMessage("hello there"),
		ai.fauxAssistantMessage([ai.fauxThinking("plan"), ai.fauxToolCall("read", { path: "a.txt" }, { id: "call-1" })], {
			stopReason: "toolUse",
		}),
	]);
	const model = faux.getModel();
	const context = { messages: [{ role: "user", content: "hi", timestamp: 1 }] };
	const completed = await ai.complete(model, context);
	const stream = ai.streamSimple(model, context);
	const events = [];
	for await (const event of stream) events.push(event.type);
	const streamed = await stream.result();
	const pending = faux.getPendingResponseCount();
	faux.unregister();
	const afterUnregister = await error(async () => {
		const message = await ai.complete(model, context);
		return [message.stopReason, message.errorMessage];
	});

	const imageModel = { id: "img", name: "Image", api: "test-images", provider: "local", type: "image", input: ["text"] };
	ai.registerImagesApiProvider({
		api: "test-images",
		generateImages: async (target, request, options) => ({
			api: target.api,
			provider: target.provider,
			model: target.id,
			output: [{ type: "text", text: `drew ${request.input[0].text} with ${options?.apiKey ?? "no key"}` }],
			stopReason: "stop",
			timestamp: 0,
		}),
	});
	const input = { input: [{ type: "text", text: "a cat" }] };
	const openrouter = { id: "img-or", name: "OR", api: "openrouter-images", provider: "openrouter", type: "image", input: ["text"], baseUrl: "http://127.0.0.1:9/api/v1" };

	return stable({
		faux: {
			api: faux.api,
			models: faux.models.map((entry) => [entry.id, entry.name, entry.api, entry.provider]),
			completed,
			events,
			streamed,
			pending,
			afterUnregister,
		},
		images: {
			drawn: await ai.generateImages(imageModel, input, { apiKey: "k1" }),
			mismatched: await error(() => ai.getImagesApiProvider("test-images").generateImages({ ...imageModel, api: "other" }, input)),
			unknown: await error(() => ai.generateImages({ ...imageModel, api: "nope" }, input)),
			builtin: typeof ai.getImagesApiProvider("openrouter-images")?.generateImages,
			noKey: await ai.generateImagesOpenRouter(openrouter, input),
		},
	});
}
