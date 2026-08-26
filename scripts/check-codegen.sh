#!/usr/bin/env bash
# Fails if the committed generated bindings differ from a fresh codegen run.
# This is what makes IDL→TS sync automatic: a schema change that wasn't
# regenerated (or a hand-edit to generated/) cannot merge.
set -euo pipefail
cd "$(dirname "$0")/.."

node codegen.mjs >/dev/null 2>&1

if ! git diff --quiet -- ts/src/generated; then
  echo "ERROR: ts/src/generated is out of date with idl/archer_v1.json."
  echo "Run \`node codegen.mjs\` and commit the result."
  echo
  git --no-pager diff --stat -- ts/src/generated
  exit 1
fi
echo "OK: generated bindings match the IDL."
