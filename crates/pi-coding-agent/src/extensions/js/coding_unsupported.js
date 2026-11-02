// Members of `@earendil-works/pi-coding-agent` that need host functionality pi-rust does not
// expose to extensions: the CLI and run modes, the extension runtime, and the interactive UI.
import { unsupportedClass, unsupportedFunction } from "./unsupported.js";

export const createAgentSessionFromServices = unsupportedFunction("createAgentSessionFromServices");
export const createAgentSessionRuntime = unsupportedFunction("createAgentSessionRuntime");
export const createAgentSessionServices = unsupportedFunction("createAgentSessionServices");
export const createCodemodeExtension = unsupportedFunction("createCodemodeExtension");
export const createExtensionRuntime = unsupportedFunction("createExtensionRuntime");
export const createMcpExtension = unsupportedFunction("createMcpExtension");
export const createToolSearchExtension = unsupportedFunction("createToolSearchExtension");
export const discoverAndLoadExtensions = unsupportedFunction("discoverAndLoadExtensions");
export const main = unsupportedFunction("main");
export const parseArgs = unsupportedFunction("parseArgs");
export const readStoredCredential = unsupportedFunction("readStoredCredential");
export const resolveCliModel = unsupportedFunction("resolveCliModel");
export const resolveModelScopeWithDiagnostics = unsupportedFunction("resolveModelScopeWithDiagnostics");
export const runPrintMode = unsupportedFunction("runPrintMode");
export const runRpcMode = unsupportedFunction("runRpcMode");

export const AgentSession = unsupportedClass("AgentSession");
export const AgentSessionRuntime = unsupportedClass("AgentSessionRuntime");
export const DefaultPackageManager = unsupportedClass("DefaultPackageManager");
export const ExtensionRunner = unsupportedClass("ExtensionRunner");
export const InteractiveMode = unsupportedClass("InteractiveMode");
export const RpcClient = unsupportedClass("RpcClient");
