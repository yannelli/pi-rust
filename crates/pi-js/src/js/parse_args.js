// util.parseArgs, following Node's lib/internal/util/parse_args.
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");

const own = (object, key) => (Object.prototype.hasOwnProperty.call(object, key) ? object[key] : undefined);
const optionGet = (options, name, key) => (Object.prototype.hasOwnProperty.call(options, name) ? own(options[name], key) : undefined);

function unknownOption(option, allowPositionals) {
  const hint = allowPositionals ? `. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- ${JSON.stringify(option)}` : "";
  return codeError(TypeError, "ERR_PARSE_ARGS_UNKNOWN_OPTION", `Unknown option '${option}'${hint}`);
}
const invalidValue = (message) => codeError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", message);

function mainArgs() {
  const execArgv = process.execArgv;
  if (execArgv.includes("-e") || execArgv.includes("--eval") || execArgv.includes("-p") || execArgv.includes("--print")) return process.argv.slice(1);
  return process.argv.slice(2);
}

function longForShort(short, options) {
  for (const name of Object.keys(options)) if (own(options[name], "short") === short) return name;
  return short;
}

const isLoneShort = (arg) => arg.length === 2 && arg[0] === "-" && arg[1] !== "-";
const isLoneLong = (arg) => arg.length > 2 && arg.startsWith("--") && !arg.includes("=", 3);
const isLongAndValue = (arg) => arg.length > 2 && arg.startsWith("--") && arg.includes("=", 3);
const isOptionLikeValue = (value) => value != null && value.length > 1 && value[0] === "-";
const isShortStart = (arg) => arg.length > 2 && arg[0] === "-" && arg[1] !== "-";
const isShortGroup = (arg, options) => isShortStart(arg) && optionGet(options, longForShort(arg[1], options), "type") !== "string";
const isShortAndValue = (arg, options) => isShortStart(arg) && optionGet(options, longForShort(arg[1], options), "type") === "string";

function argsToTokens(args, options) {
  const tokens = [];
  let index = -1;
  let groupCount = 0;
  const remaining = args.slice();
  while (remaining.length > 0) {
    const arg = remaining.shift();
    const next = remaining[0];
    if (groupCount > 0) groupCount--;
    else index++;
    if (arg === "--") {
      tokens.push({ kind: "option-terminator", index });
      for (const value of remaining) tokens.push({ kind: "positional", index: ++index, value });
      break;
    }
    if (isLoneShort(arg)) {
      const name = longForShort(arg[1], options);
      let value;
      let inlineValue;
      if (optionGet(options, name, "type") === "string" && next != null) {
        value = remaining.shift();
        inlineValue = false;
      }
      tokens.push({ kind: "option", name, rawName: arg, index, value, inlineValue });
      if (value !== undefined) ++index;
      continue;
    }
    if (isShortGroup(arg, options)) {
      const expanded = [];
      for (let i = 1; i < arg.length; i++) {
        const name = longForShort(arg[i], options);
        if (optionGet(options, name, "type") !== "string" || i === arg.length - 1) expanded.push(`-${arg[i]}`);
        else {
          expanded.push(`-${arg.slice(i)}`);
          break;
        }
      }
      remaining.unshift(...expanded);
      groupCount = expanded.length;
      continue;
    }
    if (isShortAndValue(arg, options)) {
      const name = longForShort(arg[1], options);
      tokens.push({ kind: "option", name, rawName: `-${arg[1]}`, index, value: arg.slice(2), inlineValue: true });
      continue;
    }
    if (isLoneLong(arg)) {
      const name = arg.slice(2);
      let value;
      let inlineValue;
      if (optionGet(options, name, "type") === "string" && next != null) {
        value = remaining.shift();
        inlineValue = false;
      }
      tokens.push({ kind: "option", name, rawName: arg, index, value, inlineValue });
      if (value !== undefined) ++index;
      continue;
    }
    if (isLongAndValue(arg)) {
      const equals = arg.indexOf("=");
      const name = arg.slice(2, equals);
      tokens.push({ kind: "option", name, rawName: `--${name}`, index, value: arg.slice(equals + 1), inlineValue: true });
      continue;
    }
    tokens.push({ kind: "positional", index, value: arg });
  }
  return tokens;
}

function checkUsage(config, token) {
  let name = token.name;
  if (!Object.prototype.hasOwnProperty.call(config.options, name)) {
    const negated = config.allowNegative && name.startsWith("no-") && optionGet(config.options, name.slice(3), "type") === "boolean";
    if (!negated) throw unknownOption(token.rawName, config.allowPositionals);
    name = name.slice(3);
  }
  const short = optionGet(config.options, name, "short");
  const shortAndLong = `${short ? `-${short}, ` : ""}--${name}`;
  const type = optionGet(config.options, name, "type");
  if (type === "string" && typeof token.value !== "string") throw invalidValue(`Option '${shortAndLong} <value>' argument missing`);
  if (type === "boolean" && token.value != null) throw invalidValue(`Option '${shortAndLong}' does not take an argument`);
}

function checkOptionLikeValue(token) {
  if (token.inlineValue || !isOptionLikeValue(token.value)) return;
  const example = token.rawName.startsWith("--") ? `'${token.rawName}=-XYZ'` : `'--${token.name}=-XYZ' or '${token.rawName}-XYZ'`;
  throw invalidValue(`Option '${token.rawName}' argument is ambiguous.\nDid you forget to specify the option argument for '${token.rawName}'?\nTo specify an option argument starting with a dash use ${example}.`);
}

function store(name, value, options, values, allowNegative) {
  if (name === "__proto__") return;
  if (allowNegative && name.startsWith("no-") && value === undefined) {
    name = name.slice(3);
    value = false;
  }
  const newValue = value ?? true;
  if (optionGet(options, name, "multiple")) {
    if (values[name] === undefined) values[name] = [newValue];
    else values[name].push(newValue);
  } else values[name] = newValue;
}

function validateDefault(value, name, type, multiple) {
  const check = type === "string" ? validate.string : validate.boolean;
  if (!multiple) return check(value, name);
  validate.array(value, name);
  value.forEach((item, index) => check(item, `${name}[${index}]`));
}

function parseArgs(config = {}) {
  const args = own(config, "args") ?? mainArgs();
  const strict = own(config, "strict") ?? true;
  const allowPositionals = own(config, "allowPositionals") ?? !strict;
  const returnTokens = own(config, "tokens") ?? false;
  const allowNegative = own(config, "allowNegative") ?? false;
  const options = own(config, "options") ?? { __proto__: null };
  validate.array(args, "args");
  validate.boolean(strict, "strict");
  validate.boolean(allowPositionals, "allowPositionals");
  validate.boolean(returnTokens, "tokens");
  validate.boolean(allowNegative, "allowNegative");
  validate.object(options, "options");
  for (const [name, option] of Object.entries(options)) {
    validate.object(option, `options.${name}`);
    const type = own(option, "type");
    if (type !== "string" && type !== "boolean") {
      throw invalidArgType(`options.${name}.type`, "('string|boolean')", type);
    }
    if (Object.prototype.hasOwnProperty.call(option, "short")) {
      validate.string(option.short, `options.${name}.short`);
      if (option.short.length !== 1) throw invalidArgValue(`options.${name}.short`, option.short, "must be a single character");
    }
    const multiple = own(option, "multiple");
    if (Object.prototype.hasOwnProperty.call(option, "multiple")) validate.boolean(multiple, `options.${name}.multiple`);
    const fallback = own(option, "default");
    if (fallback !== undefined) validateDefault(fallback, `options.${name}.default`, type, multiple);
  }
  const tokens = argsToTokens(args, options);
  const result = { values: { __proto__: null }, positionals: [] };
  if (returnTokens) result.tokens = tokens;
  const parseConfig = { options, allowPositionals, allowNegative };
  for (const token of tokens) {
    if (token.kind === "option") {
      if (strict) {
        checkUsage(parseConfig, token);
        checkOptionLikeValue(token);
      }
      store(token.name, token.value, options, result.values, allowNegative);
    } else if (token.kind === "positional") {
      if (!allowPositionals) {
        throw codeError(TypeError, "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL", `Unexpected argument '${token.value}'. This command does not take positional arguments`);
      }
      result.positionals.push(token.value);
    }
  }
  for (const [name, option] of Object.entries(options)) {
    const fallback = own(option, "default");
    if (fallback !== undefined && result.values[name] === undefined && name !== "__proto__") result.values[name] = fallback;
  }
  return result;
}

module.exports = { parseArgs };
