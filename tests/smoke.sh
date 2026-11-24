#!/usr/bin/env bash
# End-to-end check of a built pw: recording, the single-recorder lock,
# report output, argument errors, and (where the OS keeps reaped-children CPU
# counters) that the CPU of short-lived subprocesses reaches the program that
# ran them.
#
# Usage: tests/smoke.sh path/to/pw
set -euo pipefail

bin="$(cd "$(dirname "${1:?usage: tests/smoke.sh path/to/pw}")" && pwd)/$(basename "$1")"
work="$(mktemp -d)"
data="$work/data"
recorder=""
cleanup() {
    if [ -n "$recorder" ]; then kill "$recorder" 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $*"; }

windows=0
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) windows=1 ;; esac

"$bin" --version | grep -q '^pw ' || fail "--version"
"$bin" --help | grep -q 'Usage:' || fail "--help"
pass "help and version"

status=0
"$bin" --sort disk -d "$data" 2>"$work/err" || status=$?
[ "$status" -eq 2 ] || fail "a bad --sort exits 2, got $status"
grep -q 'takes peak-ram, peak-cpu' "$work/err" || fail "a bad --sort names the choices"
status=0
"$bin" -d "$data" 2>"$work/err" || status=$?
[ "$status" -eq 1 ] || fail "a report with nothing recorded exits 1, got $status"
grep -q 'nothing recorded' "$work/err" || fail "an empty report says nothing was recorded"
status=0
"$bin" record -i 500ms -d "$data" 2>"$work/err" || status=$?
[ "$status" -eq 2 ] || fail "a recorder interval without a unit it knows exits 2, got $status"
pass "argument and empty-data errors"

"$bin" record -i 1s -d "$data" 2>"$work/recorder.log" &
recorder=$!
sleep 3
status=0
"$bin" record -d "$data" 2>"$work/err" || status=$?
[ "$status" -eq 1 ] && grep -q 'already writing' "$work/err" || fail "a second recorder is refused"
pass "one recorder per data folder"

# Each job runs in a process group of its own, so the workload is a program
# of its own. It runs for three sample intervals, so the recorder sees it by
# name, while every `sh -c` it starts lives far shorter than one interval.
expected_ns=""
if [ "$windows" -eq 0 ]; then
    cp "$(command -v bash)" "$work/pi-burner"
    set -m
    "$work/pi-burner" -c '
        end=$((SECONDS + 3))
        while [ "$SECONDS" -lt "$end" ]; do sh -c "head -c 2000000 /dev/zero | cksum > /dev/null"; done
        times' > "$work/times" &
    wait $!
    set +m
    # `times` prints the shell'"'"'s own, then its reaped children'"'"'s, user and
    # system time as NmS.SSSs pairs.
    expected_ns=$(awk 'NR <= 2 {
            for (f = 1; f <= 2; f++) { split($f, t, /[ms]/); total += t[1] * 60 + t[2] }
        } END { printf "%d", total * 1e9 }' "$work/times")
fi
sleep 3

# The recorder keeps the minute in progress on disk after every sample, so
# killing it outright loses nothing a report needs.
kill "$recorder"
status=0
wait "$recorder" || status=$?
[ "$status" -ne 0 ] || fail "the recorder was still to be stopped"
recorder=""
pass "recording"

"$bin" -d "$data" > "$work/native.out"
grep -q '^PROGRAM ' "$work/native.out" || fail "the report has a table header"
rows=$("$bin" -d "$data" -s cpu -n 3 | awk 'f { n++ } /^PROGRAM / { f = 1 } END { print n + 0 }')
[ "$rows" -ge 1 ] && [ "$rows" -le 3 ] || fail "-n 3 shows 1 to 3 programs, got $rows"
"$bin" -d "$data" -b | grep -q '^PROGRAM  *RUNS ' || fail "-b adds a RUNS column"
"$bin" -d "$data" -b | grep -q '^[^ ].*  [1-9][0-9]*  *[1-9]' || fail "-b counts each program's runs"
pass "report"

"$bin" -c | grep -q '^Now: CPU over 1s, ' || fail "-c samples the current moment"
"$bin" -cb | grep -q '^PROGRAM  *RUNS  *PROCS  *CPU  *RAM$' || fail "-c -b adds a RUNS column"
"$bin" -c -s cpu -t '<100000%' | grep -q '^PROGRAM ' || fail "every program is below 100000% CPU now"
pass "current"

"$bin" -d "$data" -t '<1tb' | grep -q '^PROGRAM ' || fail "every program is below 1 TiB of RAM"
"$bin" -d "$data" -s procs -t '>0' | grep -q '^PROGRAM ' || fail "every program had a process"
"$bin" -d "$data" -s cpu -t '>100000%' | grep -q '^PROGRAM ' && fail "no program is above 100000% CPU"
"$bin" -d "$data" no-such-program-name | grep -q 'matching `no-such-program-name`' ||
    fail "the heading names the pattern"
status=0
"$bin" -d "$data" -t 1gb 2>"$work/err" || status=$?
[ "$status" -eq 2 ] && grep -q 'takes > or <' "$work/err" || fail "a threshold without > or < exits 2"
pass "patterns and thresholds"

rows=$("$bin" -d "$data" -p 1d -n 1000 | awk 'f { n++ } /^PROGRAM / { f = 1 } END { print n + 0 }')
[ "$rows" -ge "$("$bin" -d "$data" -n 2 | awk 'f { n++ } /^PROGRAM / { f = 1 } END { print n + 0 }')" ] ||
    fail "a 24-hour period shows at least what a 1-hour one does"
pass "periods"

cp -R "$data" "$work/kept"
"$bin" --clear -d "$work/kept" | grep -q '^pw: cleared [1-9][0-9]* recording file' ||
    fail "--clear reports what it removed"
status=0
"$bin" -d "$work/kept" 2>/dev/null || status=$?
[ "$status" -eq 1 ] || fail "a report after --clear finds nothing, exit 1; got $status"
pass "clear"

read_store() {
    case "$1" in
        *.gz) gzip -dc "$1" ;;
        *) cat "$1" ;;
    esac
}

if [ -n "$expected_ns" ]; then
    # Recordings keep CPU in ms, and an hour that has ended is gzip-compressed.
    # A bucket can sit in both an hour file and minute.tsv; count each once.
    shopt -s nullglob
    stores=("$data"/*.tsv "$data"/*.tsv.gz)
    shopt -u nullglob
    recorded_ns=$(for store in "${stores[@]}"; do read_store "$store"; done | awk -F'\t' '
        $1 == "B" { key = $2 "/" $3; skip = (key in seen); seen[key] = 1 }
        $1 == "G" && $7 == "pi-burner" && !skip { total += $2 }
        END { printf "%d", total * 1000000 }')
    # Clock ticks round each process's times; allow 5% and 50 ms.
    awk -v got="$recorded_ns" -v want="$expected_ns" 'BEGIN {
        diff = got - want; if (diff < 0) diff = -diff
        exit !(diff <= want * 0.05 + 50000000)
    }' || {
        for store in "${stores[@]}"; do
            echo "${store##*/}:" >&2
            read_store "$store" | awk -F'\t' '$1 == "B" { b = $0 } $1 == "G" && $7 == "pi-burner" { print "  " b; print "  " $0 }' >&2
        done
        fail "pi-burner recorded $recorded_ns ns of CPU, measured $expected_ns ns"
    }
    pass "short-lived subprocess CPU: recorded $recorded_ns ns, measured $expected_ns ns"
fi

echo "smoke: all checks passed"
