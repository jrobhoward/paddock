#!/usr/bin/env bash
# Regenerate the SDK's bindings (internal/gen) from wit/world.wit with the
# pinned wit-bindgen-go 0.7.0. The bindings are committed, so building an
# app needs TinyGo only; run this after a change to the WIT.
#
# Two passes: wit-bindgen-go 0.7.0 ignores --world once bulkhead:sql is
# among the packages (it then generates every world, and fails on
# wasi:clocks, which has none), so the world is generated without
# bulkhead:sql, and bulkhead:sql from a world of its own. TinyGo builds
# against wit/world.wit as it is, bulkhead:sql included.
# wit-bindgen-go is found on PATH, or set by WIT_BINDGEN_GO.
set -euo pipefail

SDK=$(cd "$(dirname "$0")" && pwd)
GEN=${WIT_BINDGEN_GO:-wit-bindgen-go}
ROOT=github.com/jrobhoward/bulkhead/sdks/go/internal/gen
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Pass 1: the world without bulkhead:sql.
cp -rL "$SDK/wit" "$work/main"
rm -r "$work/main/deps/bulkhead-sql"
grep -v 'import bulkhead:sql/' "$SDK/wit/world.wit" > "$work/main/world.wit"
"$GEN" generate --world bulkhead:go-sdk/app --out "$work/gen" --package-root "$ROOT" "$work/main"

# Pass 2: bulkhead:sql alone.
mkdir -p "$work/sql/deps/bulkhead-sql"
cp "$SDK/wit/deps/bulkhead-sql/sql.wit" "$work/sql/deps/bulkhead-sql/"
printf 'package bulkhead:go-sdk-sql;\n\nworld sql {\n  import bulkhead:sql/sql@0.1.0;\n}\n' > "$work/sql/world.wit"
"$GEN" generate --world bulkhead:go-sdk-sql/sql --out "$work/sqlgen" --package-root "$ROOT" "$work/sql"
cp -r "$work/sqlgen/bulkhead/sql" "$work/gen/bulkhead/"

rm -rf "$SDK/internal/gen"
mv "$work/gen" "$SDK/internal/gen"
