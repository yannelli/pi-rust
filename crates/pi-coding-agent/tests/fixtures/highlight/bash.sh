#!/usr/bin/env bash
set -euo pipefail

# Greet everyone
greet() {
  local name="${1:-world}"
  echo "Hello $name" > /dev/null
}

for i in 1 2 3; do
  if [[ -n "$i" && $i -gt 1 ]]; then
    greet "user$i"
  fi
done
export PATH="$HOME/bin:$PATH"
