// JSON.parse cases run unchanged by real Node (to write json_parse_node.json) and by pi-js.
// Messages go through JSON.stringify so a lone surrogate survives the Rust JSON decoder.
"use strict";

const long = " ".repeat(26);
const syntax = {
  bom: "﻿{}",
  bomPath: '﻿{"/stable/source/path":true}',
  nbsp: " {}",
  cjk: "中{}",
  emoji: "\u{1f600}{}",
  longStart: `é${long}`,
  longMiddle: `${" ".repeat(14)}é${" ".repeat(14)}`,
  longEnd: `${long}é`,
  objectKey: '{"a"é:1}',
  objectValue: '{"a":é}',
  arrayComma: "[1é]",
  trailing: "{}é",
  unicodeKeyThenComma: '{"é\u{1f600}":1,é}',
  unicodeStringThenToken: '["中文", x]',
  unicodeValueThenSpace: '{"é": 1 2}',
  unicodeStringTrailing: '"é\u{1f600}" x',
  escapeLatin1: '"\\é"',
  escapeOver255: '"\\中"',
  escapeAtEnd: '"\\',
  ascii: "x",
  shortLiteral: "tru",
  literalMismatch: "nulx",
  literalDigit: "nul1",
  literalQuote: 'nu"',
  capital: "True",
  arrayTrailingComma: "[1,]",
  objectLeadingComma: "{,}",
  objectNumberKey: "{1:2}",
  leadingZero: "01",
  minusOnly: "-",
  minusLetter: "-a",
  fraction: "1.",
  exponent: "1e+",
  unterminated: '"abc',
  badEscape: '"\\x"',
  badUnicodeEscape: '"\\u12g4"',
  controlCharacter: '"a\u0001"',
  colonValue: ":",
  undefined: "undefined",
  nan: "NaN",
  infinity: "Infinity",
  objectObject: "[object Object]",
  empty: "",
  whitespace: "  \n ",
  openBrace: "{",
  openArray: "[1",
  newlineGrammar: '{\n"a":1,\r\n"b" 2}',
  carriageReturns: "[\r1\r\n,é]",
  unicodeEarlierLine: '["é\u{1f600}",\n x]',
  longGrammar: `{"${"é".repeat(30)}" 1}`,
  longTrailing: `[${"1,".repeat(20)}1] [`,
};

function outcome(run) {
  try {
    return { value: JSON.stringify(run()) };
  } catch (error) {
    return { error: error && error.name, message: JSON.stringify(error && error.message) };
  }
}

module.exports = function cases() {
  const results = {};
  for (const [name, input] of Object.entries(syntax)) results[name] = outcome(() => JSON.parse(input));
  results.objectArgument = outcome(() => JSON.parse({}));
  results.reviver = outcome(() => JSON.parse('{"a":[1,2,{"b":null}],"é":3}', (key, value) => (typeof value === "number" ? value * 2 : value)));
  let coercions = 0;
  const counted = { toString: () => (coercions++, "[1]") };
  results.coercedOnce = { ...outcome(() => JSON.parse(counted, (key, value) => value)), coercions };
  for (const [name, mimic] of [["reviverFake", "Unexpected token '\\ufeff' in JSON"], ["reviverFakeQuickJs", "unexpected token: 'x'"]]) {
    const fake = new SyntaxError(mimic);
    let caught;
    try {
      JSON.parse("[1]", () => {
        throw fake;
      });
    } catch (error) {
      caught = error;
    }
    results[name] = { same: caught === fake, message: JSON.stringify(caught.message) };
  }
  const coerceFake = new SyntaxError("Unexpected end of JSON input");
  let coerceCaught;
  try {
    JSON.parse({ toString: () => {
      throw coerceFake;
    } });
  } catch (error) {
    coerceCaught = error;
  }
  results.coercerFake = { same: coerceCaught === coerceFake, message: JSON.stringify(coerceCaught.message) };
  let constructs;
  try {
    new JSON.parse("1");
    constructs = "constructed";
  } catch (error) {
    constructs = error.name;
  }
  results.function = { name: JSON.parse.name, length: JSON.parse.length, constructs };
  return results;
};

if (require.main === module) {
  const golden = { node: process.version, cases: module.exports() };
  process.stdout.write(`${JSON.stringify(golden, null, "\t")}\n`);
}
