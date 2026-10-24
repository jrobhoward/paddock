#!/usr/bin/env bash
# Run the checks a change must pass (CLAUDE.md, "Definition of Done"), each
# with a one-line summary, and exit non-zero if any fails. CI runs the same
# checks through this script, one or a few per job.
#
# Usage:
#   scripts/check.sh                    every check
#   scripts/check.sh --quick            fmt, clippy, conventions and the tests
#   scripts/check.sh CHECK...           the named checks only
#   scripts/check.sh --list             the check names
#
# Options:
#   --base REF      the branch a change is compared with for `changelog`
#                   (default: the upstream of the current branch; without
#                   one, `changelog` is skipped)
#   -v, --verbose   stream each check's output (the default under CI)
#
# Each check's output goes to target/check/<name>.log; a failure prints the
# end of its log after the summary. Exit status: 0 when every check passed or
# was skipped, 1 when one failed, 2 on a usage error.
#
# Checks:
#   fmt          cargo fmt --check
#   clippy       clippy on every target and feature, -Dwarnings
#   build        cargo build --workspace --all-targets: links what clippy and
#                the tests do not (the guest crates' native cdylibs)
#   cli-no-node  paddock without its `node` feature builds and has no wasmtime
#   features     the node with no optional feature (no HTTP ingress, TLS,
#                probes or Modbus), and with HTTP but no TLS, each builds
#                clean; the first has none of their dependencies
#   test         cargo test --workspace --all-features (needs the
#                wasm32-wasip2 target)
#   doc          rustdoc with warnings as errors
#   deny         cargo-deny: licences, bans, sources, advisories
#   msrv         cargo check on the workspace's rust-version
#   guests       every guest crate built for wasm32-wasip2, each checked with
#                wasm-tools for the app world or the connection handler, and
#                the bundle fixture test
#   secrets      scripts/check-secrets.sh (with --push when there is an upstream)
#   conventions  what clippy does not check: test names
#                (subject____condition____result), tests in *_tests.rs files
#                registered with #[cfg(test)] #[path], no inline test modules,
#                the #![allow] header on each test file
#   changelog    a change to crates/, examples/ or wit/ also changes
#                CHANGELOG.md, unless a commit since --base has a line
#                `Changelog: none`

set -uo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

all_checks="fmt clippy build cli-no-node features test doc deny msrv guests secrets conventions changelog"
quick_checks="fmt clippy conventions test"

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

checks=""
base=""
verbose=0
[ -n "${CI:-}" ] && verbose=1
while [ "$#" -gt 0 ]; do
    case "$1" in
        --quick) checks="$checks $quick_checks" ;;
        --base) [ "$#" -ge 2 ] || { echo "check: --base needs a ref" >&2; exit 2; }; base=$2; shift ;;
        --base=*) base=${1#--base=} ;;
        -v | --verbose) verbose=1 ;;
        --list) echo "$all_checks" | tr ' ' '\n'; exit 0 ;;
        -h | --help) usage; exit 0 ;;
        -*) echo "check: unknown option $1" >&2; exit 2 ;;
        *)
            case " $all_checks " in
                *" $1 "*) checks="$checks $1" ;;
                *) echo "check: unknown check $1 (--list shows them)" >&2; exit 2 ;;
            esac ;;
    esac
    shift
done
[ -n "$checks" ] || checks=$all_checks

logdir=target/check
mkdir -p "$logdir"

# A check prints why it was skipped and returns this.
SKIP=3

# `grep -q` that reads all of its input. Under pipefail, `cmd | grep -q`
# fails when grep exits early and cmd is killed by SIGPIPE.
has() {
    grep "$@" > /dev/null
}

need() { # command, how to install it
    command -v "$1" > /dev/null 2>&1 && return 0
    echo "missing: $1 ($2)"
    return 1
}

need_target() {
    if command -v rustup > /dev/null 2>&1 && ! rustup target list --installed | has -x "$1"; then
        echo "missing: the $1 target (rustup target add $1)"
        return 1
    fi
}

# Workspace members, from the root Cargo.toml.
members() {
    awk '/^members = \[/ { on = 1; next } on && /^\]/ { exit } on { gsub(/[ ",]/, ""); if ($0 != "") print }' Cargo.toml
}

# The [package] name in a Cargo.toml.
package_name() {
    awk '/^\[package\]/ { on = 1; next } /^\[/ { on = 0 } on && /^name *=/ { gsub(/.*= *"|".*/, ""); print; exit }' "$1"
}

check_fmt() {
    cargo fmt --all -- --check
}

check_clippy() {
    cargo clippy --workspace --all-targets --all-features --locked -- -Dwarnings
}

check_build() {
    cargo build --workspace --all-targets --locked
}

# `paddock` without its `node` feature is the CLI and bundle tooling only; it
# must build, and must not pull in wasmtime.
check_cli_no_node() {
    cargo clippy -p paddock --no-default-features --all-targets --locked -- -Dwarnings || return 1
    if cargo tree -p paddock --no-default-features -e normal --locked | has wasmtime; then
        echo "paddock --no-default-features depends on wasmtime"
        return 1
    fi
}

# Each optional capability compiles out: `paddock` with `node` alone, and
# `paddock-host` with no features, then the HTTP ingress without TLS. The
# default and every feature together are `build` and `clippy`.
check_features() {
    cargo clippy -p paddock-host --no-default-features --all-targets --locked -- -Dwarnings || return 1
    cargo clippy -p paddock --no-default-features --features node --all-targets --locked -- -Dwarnings || return 1
    cargo clippy -p paddock --no-default-features --features http --all-targets --locked -- -Dwarnings || return 1
    local found
    found=$(cargo tree -p paddock --no-default-features --features node -e normal --prefix none --locked |
        grep -E '^(paddock-modbus|tokio-modbus|anyprobe|tokio-rustls) ' | sort -u)
    if [ -n "$found" ]; then
        echo "paddock with only the node feature depends on:"
        echo "$found"
        return 1
    fi
}

# The host tests build the guest fixtures themselves, so they need the target.
# Every feature, so the Modbus tests run too.
check_test() {
    need_target wasm32-wasip2 || return 1
    cargo test --workspace --all-features --locked --no-fail-fast
}

check_doc() {
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
}

check_deny() {
    cargo deny --version > /dev/null 2>&1 || { echo "missing: cargo-deny (cargo install cargo-deny)"; return 1; }
    cargo deny check licenses bans sources advisories
}

check_msrv() {
    local msrv
    msrv=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
    [ -n "$msrv" ] || { echo "no rust-version in Cargo.toml"; return 1; }
    rustup run "$msrv" rustc --version > /dev/null 2>&1 \
        || { echo "missing: toolchain $msrv (rustup toolchain install $msrv)"; return 1; }
    cargo "+$msrv" check --workspace --all-targets --all-features --locked
}

check_guests() {
    need_target wasm32-wasip2 || return 1
    need wasm-tools "cargo install wasm-tools" || return 1
    local dir name packages="" wasm wit f failed=0
    for dir in $(members); do
        grep -q 'crate-type = \["cdylib"\]' "$dir/Cargo.toml" || continue
        # A native library (a rustbridge plugin), not a guest.
        grep -q '^native-library = true' "$dir/Cargo.toml" && continue
        packages="$packages $(package_name "$dir/Cargo.toml")"
    done
    echo "guests:$packages"
    local args=""
    for name in $packages; do args="$args -p $name"; done
    # shellcheck disable=SC2086
    cargo build $args --target wasm32-wasip2 --release --locked || return 1
    for name in $packages; do
        wasm=target/wasm32-wasip2/release/$(echo "$name" | tr - _).wasm
        wasm-tools validate --features all "$wasm" || { failed=1; continue; }
        wit=$(wasm-tools component wit "$wasm") || { failed=1; continue; }
        # Connection handlers export `paddock:connection/handler`. Apps export
        # the app world's callbacks; a component imports only what its code
        # calls (echo has no `process` import), so only exports are checked.
        if echo "$wit" | has "export paddock:connection/handler@"; then
            echo "$name: connection handler"
            continue
        fi
        # HTTP handlers export `wasi:http/incoming-handler` (ROADMAP E6).
        if echo "$wit" | has "export wasi:http/incoming-handler@0.2."; then
            echo "$name: HTTP handler"
            continue
        fi
        for f in init handle-call handle-cast handle-info terminate save-snapshot load-snapshot; do
            echo "$wit" | has "export $f: func" || { echo "$name: missing export $f"; failed=1; }
        done
        echo "$name: app"
    done
    [ "$failed" = 0 ] || return 1
    # Tests that load a built fixture are ignored in the plain test run.
    cargo test -p paddock-bundle --test fixture_echo --locked -- --ignored
}

check_secrets() {
    if git rev-parse -q --verify '@{upstream}' > /dev/null 2>&1; then
        scripts/check-secrets.sh --push
    else
        scripts/check-secrets.sh
    fi
}

# Rust files the conventions apply to: the main workspace, not spikes or fuzz.
rust_files() {
    git ls-files -- '*.rs' ':!spikes/' ':!fuzz/'
}

check_conventions() {
    local found=0 f base dir out

    # Test names: subject____condition____result.
    out=$(rust_files | xargs awk '
        /^[[:space:]]*#\[(test|tokio::test|rstest|test_case)([](:]|$)/ { pending = 1; next }
        pending && /^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]/ {
            s = $0; sub(/.*fn[[:space:]]+/, "", s); sub(/[^A-Za-z0-9_].*/, "", s)
            print FILENAME ":" FNR "\t" s; pending = 0
        }' | grep -v -E '	[A-Za-z0-9]+(_[A-Za-z0-9]+)*(____[A-Za-z0-9]+(_[A-Za-z0-9]+)*){2}$')
    if [ -n "$out" ]; then
        found=1
        echo "test names not of the form subject____condition____result:"
        echo "$out" | sed 's/^/  /'
    fi

    # Each *_tests.rs file is registered beside its source file with
    # #[cfg(test)] #[path = "..."].
    for f in $(git ls-files -- '*_tests.rs' ':!spikes/' ':!fuzz/'); do
        base=$(basename "$f")
        dir=$(dirname "$f")
        if ! grep -B1 -F "#[path = \"$base\"]" "$dir"/*.rs 2> /dev/null | has -F '#[cfg(test)]'; then
            found=1
            echo "$f: not registered with #[cfg(test)] #[path = \"$base\"] in $dir"
        fi
    done

    # No inline test modules: #[cfg(test)] on `mod name {`.
    out=$(rust_files | xargs awk '
        /^[[:space:]]*#\[cfg\(test\)\]/ { pending = 1; next }
        pending && /^[[:space:]]*#\[/ { next }
        pending && /^[[:space:]]*(pub[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{/ { print FILENAME ":" FNR ": " $0 }
        { pending = 0 }')
    if [ -n "$out" ]; then
        found=1
        echo "inline test modules (move them to a *_tests.rs file):"
        echo "$out" | sed 's/^/  /'
    fi

    # The #![allow] header: unit test files and integration test crate roots.
    out=$( { git ls-files -- '*_tests.rs' ':!spikes/' ':!fuzz/'
             git ls-files -- ':(glob)**/tests/*.rs' ':!spikes/' ':!fuzz/'
             git ls-files -- ':(glob)**/tests/*/main.rs' ':!spikes/' ':!fuzz/'; } | sort -u | xargs awk '
        function finish() {
            if (file != "" && !(seen["non_snake_case"] && seen["clippy::unwrap_used"] && seen["clippy::expect_used"]))
                print file
        }
        FNR == 1 { finish(); file = FILENAME; delete seen; header = 1 }
        !header { next }
        /^[[:space:]]*$/ || /^[[:space:]]*\/\// { next }
        /^#!\[allow\(/ {
            s = $0; sub(/^#!\[allow\(/, "", s); sub(/\)\].*/, "", s)
            n = split(s, parts, ",")
            for (i = 1; i <= n; i++) { p = parts[i]; gsub(/[[:space:]]/, "", p); seen[p] = 1 }
            next
        }
        /^#!\[/ { next }
        { header = 0 }
        END { finish() }')
    if [ -n "$out" ]; then
        found=1
        echo "test files without the header #![allow(non_snake_case)], #![allow(clippy::unwrap_used)], #![allow(clippy::expect_used)]:"
        echo "$out" | sed 's/^/  /'
    fi

    [ "$found" = 0 ] && echo "conventions hold"
    [ "$found" = 0 ]
}

check_changelog() {
    local ref=$base mb changed
    if [ -z "$ref" ]; then
        ref=$(git rev-parse -q --abbrev-ref '@{upstream}' 2> /dev/null) || ref=""
    fi
    if [ -z "$ref" ]; then
        echo "no --base and no upstream branch to compare with"
        return "$SKIP"
    fi
    mb=$(git merge-base "$ref" HEAD) || { echo "no merge base with $ref"; return 1; }
    # Committed and uncommitted changes since the merge base, and new files.
    changed=$( { git diff --name-only "$mb"; git ls-files --others --exclude-standard; } | sort -u)
    echo "compared with $ref ($(git rev-parse --short "$mb"))"
    if ! echo "$changed" | has -E '^(crates|examples|wit)/'; then
        echo "no change to crates/, examples/ or wit/"
        return 0
    fi
    if echo "$changed" | has -x 'CHANGELOG.md'; then
        echo "CHANGELOG.md changed"
        return 0
    fi
    if git log --format=%B "$mb..HEAD" | has -x -i 'Changelog: none'; then
        echo "a commit says Changelog: none"
        return 0
    fi
    echo "crates/, examples/ or wit/ changed but CHANGELOG.md did not."
    echo "Add an entry, or a line 'Changelog: none' to a commit message when nothing a user would notice changed."
    return 1
}

failed=""
summary=""
for name in $checks; do
    log=$logdir/$name.log
    start=$(date +%s)
    if [ "$verbose" = 1 ]; then
        [ -n "${GITHUB_ACTIONS:-}" ] && echo "::group::$name"
        ("check_${name//-/_}") 2>&1 | tee "$log"
        status=${PIPESTATUS[0]}
        [ -n "${GITHUB_ACTIONS:-}" ] && echo "::endgroup::"
    else
        ("check_${name//-/_}") > "$log" 2>&1
        status=$?
    fi
    secs=$(( $(date +%s) - start ))
    case "$status" in
        0) line=$(printf 'ok    %-12s %5ss' "$name" "$secs") ;;
        "$SKIP") line=$(printf 'skip  %-12s %5ss  %s' "$name" "$secs" "$(tail -n 1 "$log")") ;;
        *) line=$(printf 'FAIL  %-12s %5ss  %s' "$name" "$secs" "$log"); failed="$failed $name" ;;
    esac
    echo "$line"
    summary="$summary$line
"
done

if [ -n "$failed" ]; then
    if [ "$verbose" = 0 ]; then
        for name in $failed; do
            echo
            echo "--- $name (end of $logdir/$name.log)"
            tail -n 30 "$logdir/$name.log"
        done
    fi
    echo
    printf '%s' "$summary"
    echo "check: failed:$failed"
    exit 1
fi
echo "check: all passed"
