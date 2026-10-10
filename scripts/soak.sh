#!/usr/bin/env bash
# Soak Paddock for hours, from random starting seeds; not run in CI. Each
# round runs, in turn:
#   sim       the simulation's random scenarios and random failover
#             schedules (paddock-host's tests/simulation, filter `random`)
#   failover  a witness and a pair as `paddock node` processes behind UDP
#             proxies, under link cuts, holds and loss, SIGKILL and SIGSTOP
#             (paddock-soak failover); fails on any moment when both nodes
#             ran the singleton, or a heal after which other than one runs it
#   chaos     the chaos example on one node under failing commands
#             (paddock-soak chaos)
#
# Usage:
#   scripts/soak.sh --hours N [options]
#
# Options:
#   --hours N             how long to run (fractions allowed); the run in
#                         progress at the deadline is shortened to fit
#   --dir DIR             where runs go (default target/soak/<UTC time>)
#   --seed N              the first seed (default: random); each run takes
#                         the next
#   --failover-minutes M  length of each failover run (default 10)
#   --chaos-minutes M     length of each chaos run (default 5)
#   --sim-seeds N         random scenarios per sim run (default 200)
#   --only KIND           run only sim, failover or chaos
#   --keep                keep the directories of runs that passed
#
# Each run gets a directory named <round>-<kind>-<seed> with its seed, logs
# and result; a run that passed is removed unless --keep. summary.txt in
# --dir lists every run. Rerun a failover or chaos run with the same seed:
#   target/release/paddock-soak failover --dir DIR --seed SEED
# (timing still differs between runs, so a seed does not replay exactly),
# and recheck a failover run's logs with paddock-soak check DIR. Replay a
# simulation seed exactly with PADDOCK_SIM_SEED=<seed> (scenarios) or
# PADDOCK_SIM_FAILOVER_FIRST_SEED=<seed> PADDOCK_SIM_FAILOVER_SEEDS=1.
#
# Exit status: 0 when every run passed, 1 when one failed, 2 on a usage error.

set -uo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

hours=""
dir=""
seed=$(( (RANDOM << 15 | RANDOM) ))
failover_minutes=10
chaos_minutes=5
sim_seeds=200
only=""
keep=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --hours) hours=${2:-}; shift ;;
        --dir) dir=${2:-}; shift ;;
        --seed) seed=${2:-}; shift ;;
        --failover-minutes) failover_minutes=${2:-}; shift ;;
        --chaos-minutes) chaos_minutes=${2:-}; shift ;;
        --sim-seeds) sim_seeds=${2:-}; shift ;;
        --only) only=${2:-}; shift ;;
        --keep) keep=1 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "soak: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
if [ -z "$hours" ]; then
    echo "soak: --hours is required (--help)" >&2
    exit 2
fi
case "$only" in
    "") kinds="sim failover chaos" ;;
    sim | failover | chaos) kinds=$only ;;
    *) echo "soak: --only takes sim, failover or chaos" >&2; exit 2 ;;
esac
[ -n "$dir" ] || dir=target/soak/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$dir"
summary=$dir/summary.txt

echo "soak: building (release)"
cargo build --release --locked -p paddock -p paddock-soak || exit 1
cargo build --release --locked --target wasm32-wasip2 -p example-counter -p example-chaos || exit 1

now() { date +%s; }
deadline=$(awk -v h="$hours" -v t="$(now)" 'BEGIN { printf "%d", t + h * 3600 }')
# Minutes left before the deadline, at most $1, at least 1.
fit() { awk -v m="$1" -v left="$(( deadline - $(now) ))" 'BEGIN { l = left / 60; if (l < m) m = l; if (m < 1) m = 1; print m }'; }

passed=0
failed=0
round=1
echo "soak: until $(date -d "@$deadline" 2>/dev/null || date -r "$deadline"), runs in $dir, first seed $seed" | tee "$summary"
while [ "$(now)" -lt "$deadline" ]; do
    for kind in $kinds; do
        [ "$(now)" -lt "$deadline" ] || break
        run=$dir/$round-$kind-$seed
        mkdir -p "$run"
        echo "$seed" > "$run/seed"
        start=$(now)
        case "$kind" in
            sim)
                PADDOCK_SIM_FIRST_SEED=$((seed * sim_seeds)) PADDOCK_SIM_SEEDS=$sim_seeds \
                PADDOCK_SIM_FAILOVER_FIRST_SEED=$((seed * sim_seeds)) PADDOCK_SIM_FAILOVER_SEEDS=$((sim_seeds / 10)) \
                    cargo test --locked -p paddock-host --test simulation random > "$run/output.txt" 2>&1
                ;;
            failover)
                target/release/paddock-soak failover --dir "$run" --seed "$seed" \
                    --minutes "$(fit "$failover_minutes")" > "$run/output.txt" 2>&1
                ;;
            chaos)
                target/release/paddock-soak chaos --dir "$run" --seed "$seed" \
                    --minutes "$(fit "$chaos_minutes")" > "$run/output.txt" 2>&1
                ;;
        esac
        status=$?
        took=$(( $(now) - start ))
        if [ "$status" = 0 ]; then
            passed=$((passed + 1))
            line=$(printf 'pass  %-8s seed %-12s %6ss' "$kind" "$seed" "$took")
            [ "$keep" = 1 ] || rm -rf "$run"
        else
            failed=$((failed + 1))
            line=$(printf 'FAIL  %-8s seed %-12s %6ss  %s' "$kind" "$seed" "$took" "$run")
        fi
        echo "$line" | tee -a "$summary"
        seed=$((seed + 1))
    done
    round=$((round + 1))
done

echo "soak: $passed passed, $failed failed; $summary" | tee -a "$summary"
[ "$failed" = 0 ]
