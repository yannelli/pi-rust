// Utilities of `@earendil-works/pi-coding-agent` outside the embedded upstream files. Skills,
// diffs, and images go to the Rust implementations; the rest is upstream code.
import { open } from "node:fs/promises";
import { call, callAsync } from "./bridge.js";
import { unsupportedError } from "./unsupported.js";

export function createSyntheticSourceInfo(path, options) {
	return {
		path,
		source: options.source,
		scope: options.scope ?? "temporary",
		origin: options.origin ?? "top-level",
		baseDir: options.baseDir,
	};
}

export class CredentialSynchronizationError extends Error {
	constructor(providerId, operation, credential, options) {
		super(`Credential ${operation} committed for ${providerId}, but local synchronization failed`, options);
		this.name = "CredentialSynchronizationError";
		this.providerId = providerId;
		this.operation = operation;
		this.credential = credential;
	}
}

export const loadSkills = (options) => call("sdk.loadSkills", options);
export const loadSkillsFromDir = (options) => call("sdk.loadSkillsFromDir", options);

const escapeXml = (text) =>
	text
		.replace(/&/g, "&amp;")
		.replace(/</g, "&lt;")
		.replace(/>/g, "&gt;")
		.replace(/"/g, "&quot;")
		.replace(/'/g, "&apos;");

export function formatSkillsForPrompt(skills, fileReadTool = "read") {
	const visibleSkills = skills.filter((skill) => !skill.disableModelInvocation);
	if (visibleSkills.length === 0) return "";
	const lines = [
		"\n\nThe following skills provide specialized instructions for specific tasks.",
		fileReadTool === "read"
			? "Use the read tool to load a skill's file when the task matches its description."
			: fileReadTool === "bash"
				? "Use bash to load a skill's file when the task matches its description."
				: "Load a skill's file when the task matches its description.",
		"When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
		"",
		"<available_skills>",
	];
	for (const skill of visibleSkills) {
		lines.push("  <skill>");
		lines.push(`    <name>${escapeXml(skill.name)}</name>`);
		lines.push(`    <description>${escapeXml(skill.description)}</description>`);
		lines.push(`    <location>${escapeXml(skill.filePath)}</location>`);
		lines.push("  </skill>");
	}
	lines.push("</available_skills>");
	return lines.join("\n");
}

function requireDefaultContext(member, contextLines) {
	if (contextLines !== undefined && contextLines !== 4) throw unsupportedError(`${member} contextLines other than 4`);
}

export function generateDiffString(oldContent, newContent, contextLines) {
	requireDefaultContext("generateDiffString", contextLines);
	const { diff, firstChangedLine } = call("sdk.generateDiffString", oldContent, newContent);
	return { diff, firstChangedLine: firstChangedLine ?? undefined };
}

export function generateUnifiedPatch(path, oldContent, newContent, contextLines) {
	requireDefaultContext("generateUnifiedPatch", contextLines);
	return call("sdk.generateUnifiedPatch", path, oldContent, newContent);
}

export const renderDiff = (diffText, _options) => call("sdk.renderDiff", diffText);

export function wrapRegisteredTool(registeredTool, runner) {
	const definition = registeredTool.definition;
	return {
		name: definition.name,
		label: definition.label,
		description: definition.description,
		parameters: definition.parameters,
		outputSchema: definition.outputSchema,
		constrainedSampling: definition.constrainedSampling,
		prepareArguments: definition.prepareArguments,
		executionMode: definition.executionMode,
		execute: (toolCallId, params, signal, onUpdate, ctx) =>
			definition.execute(toolCallId, params, signal, onUpdate, ctx ?? runner.createToolContext(toolCallId, signal)),
	};
}

export const wrapRegisteredTools = (registeredTools, runner) =>
	registeredTools.map((tool) => wrapRegisteredTool(tool, runner));

export async function convertToPng(base64Data, mimeType) {
	if (mimeType === "image/png") return { data: base64Data, mimeType };
	return (await callAsync("sdk.convertToPng", base64Data, mimeType)) ?? null;
}

export async function resizeImage(inputBytes, mimeType, options) {
	return (await callAsync("sdk.resizeImage", Array.from(inputBytes), mimeType, options ?? null)) ?? null;
}

export function formatDimensionNote(result) {
	if (!result.wasResized) return undefined;
	const scale = result.originalWidth / result.width;
	return `[Image: original ${result.originalWidth}x${result.originalHeight}, displayed at ${result.width}x${result.height}. Multiply coordinates by ${scale.toFixed(2)} to map to original image.]`;
}

const IMAGE_TYPE_SNIFF_BYTES = 4100;
const PNG_SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

const byte = (buffer, offset) => buffer[offset] ?? 0;
const readUint16LE = (buffer, offset) => byte(buffer, offset) + (byte(buffer, offset + 1) << 8);
const readUint32BE = (buffer, offset) =>
	byte(buffer, offset) * 0x1000000 + (byte(buffer, offset + 1) << 16) + (byte(buffer, offset + 2) << 8) + byte(buffer, offset + 3);
const readUint32LE = (buffer, offset) =>
	byte(buffer, offset) + (byte(buffer, offset + 1) << 8) + (byte(buffer, offset + 2) << 16) + byte(buffer, offset + 3) * 0x1000000;
const startsWith = (buffer, bytes) => buffer.length >= bytes.length && bytes.every((value, index) => buffer[index] === value);

function startsWithAscii(buffer, offset, text) {
	if (buffer.length < offset + text.length) return false;
	for (let index = 0; index < text.length; index++) {
		if (buffer[offset + index] !== text.charCodeAt(index)) return false;
	}
	return true;
}

const isPng = (buffer) =>
	buffer.length >= 16 && readUint32BE(buffer, PNG_SIGNATURE.length) === 13 && startsWithAscii(buffer, 12, "IHDR");

function isAnimatedPng(buffer) {
	let offset = PNG_SIGNATURE.length;
	while (offset + 8 <= buffer.length) {
		const chunkLength = readUint32BE(buffer, offset);
		if (startsWithAscii(buffer, offset + 4, "acTL")) return true;
		if (startsWithAscii(buffer, offset + 4, "IDAT")) return false;
		const nextOffset = offset + 8 + chunkLength + 4;
		if (nextOffset <= offset || nextOffset > buffer.length) return false;
		offset = nextOffset;
	}
	return false;
}

function isBmp(buffer) {
	if (buffer.length < 26) return false;
	const declaredFileSize = readUint32LE(buffer, 2);
	const pixelDataOffset = readUint32LE(buffer, 10);
	const dibHeaderSize = readUint32LE(buffer, 14);
	if (declaredFileSize !== 0 && declaredFileSize < 26) return false;
	if (pixelDataOffset < 14 + dibHeaderSize) return false;
	if (declaredFileSize !== 0 && pixelDataOffset >= declaredFileSize) return false;
	let planesOffset;
	if (dibHeaderSize === 12) planesOffset = 22;
	else if (dibHeaderSize >= 40 && dibHeaderSize <= 124 && buffer.length >= 30) planesOffset = 26;
	else return false;
	const bitsPerPixel = readUint16LE(buffer, planesOffset + 2);
	return readUint16LE(buffer, planesOffset) === 1 && [1, 4, 8, 16, 24, 32].includes(bitsPerPixel);
}

function detectSupportedImageMimeType(buffer) {
	if (startsWith(buffer, [0xff, 0xd8, 0xff])) return buffer[3] === 0xf7 ? null : "image/jpeg";
	if (startsWith(buffer, PNG_SIGNATURE)) return isPng(buffer) && !isAnimatedPng(buffer) ? "image/png" : null;
	if (startsWithAscii(buffer, 0, "GIF87a") || startsWithAscii(buffer, 0, "GIF89a")) return "image/gif";
	if (startsWithAscii(buffer, 0, "RIFF") && startsWithAscii(buffer, 8, "WEBP")) return "image/webp";
	if (startsWithAscii(buffer, 0, "BM") && isBmp(buffer)) return "image/bmp";
	return null;
}

export async function detectSupportedImageMimeTypeFromFile(filePath) {
	const fileHandle = await open(filePath, "r");
	try {
		const buffer = Buffer.alloc(IMAGE_TYPE_SNIFF_BYTES);
		const { bytesRead } = await fileHandle.read(buffer, 0, IMAGE_TYPE_SNIFF_BYTES, 0);
		return detectSupportedImageMimeType(buffer.subarray(0, bytesRead));
	} finally {
		await fileHandle.close();
	}
}
