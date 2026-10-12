#!/bin/sh
# Launches a separate app instance at a given window size and captures just its window.
# Needs Screen Recording permission for whatever runs this script (Terminal, T3 Code, ...).
# usage: [MASTHEAD_OPEN="Nodes/production-worker#overview"] scripts/screenshot.sh out.png [WIDTHxHEIGHT] [seconds-to-wait]
# MASTHEAD_OPEN: Kind[/name][#overview|yaml|logs|picker|menu|compare]
set -e
out=${1:?usage: $0 out.png [WIDTHxHEIGHT] [wait]}
size=${2:-1400x850}
wait=${3:-6}
dir=$(cd "$(dirname "$0")/.." && pwd)
helper="${TMPDIR:-/tmp}/masthead-winid"
[ -x "$helper" ] || swiftc -O -o "$helper" "$dir/scripts/winid.swift"
MASTHEAD_SIZE=$size "$dir/target/release/masthead" >/dev/null 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null' EXIT
sleep "$wait"
id=$("$helper" "$pid" | head -1 | cut -d' ' -f1)
screencapture -x -o -l "$id" "$out"
echo "$out"
