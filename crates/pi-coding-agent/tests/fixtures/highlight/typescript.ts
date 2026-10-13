import type { Config } from "./config";

interface Options<T> {
  name: string;
  value?: T;
}

export function parse<T extends object>(input: string, opts: Options<T>): T | undefined {
  // Parse the input
  const result: number[] = [1, 2, 3];
  if (input.length > 0 && opts.name !== "") {
    return JSON.parse(input) as T;
  }
  return undefined;
}

enum Color { Red = 1, Green }
type Pair = [string, number];
