#!/usr/bin/env bash
# Benchmark harness (spec §5).
#
# Drives the daemon's CLI front end with a set of canned requests, parses the
# `hermit_timing` lines it writes to stderr, and prints p50/p95 for every stage.
#
# Usage:
#   scripts/bench.sh [--bin PATH] [--config PATH] [--runs N] [--prompts FILE]
#
# Run it ON THE PI, over Ethernet, with the service stopped (it needs the CLI):
#   sudo systemctl stop hermit
#   sudo -u hermit PATH=$PATH scripts/bench.sh --bin /opt/hermit/bin/hermit \
#       --config /opt/hermit/config/hermit.toml
#
# Gates it reports against (all measured end-to-end, over Ethernet):
#   local harness overhead (route+recall+assemble)  <= 15 ms
#   text TTFT, no tools                              < 700 ms
#   first audio, no tools                            < 1200 ms
#   first audio, one web search                      < 2000 ms
#   fast-path device command                         < 50 ms

set -euo pipefail

BIN="${BIN:-./target/release/hermit}"
CONFIG="${CONFIG:-config/hermit.toml}"
RUNS=20
PROMPTS=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin)     BIN="$2"; shift 2 ;;
    --config)  CONFIG="$2"; shift 2 ;;
    --runs)    RUNS="$2"; shift 2 ;;
    --prompts) PROMPTS="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ ! -x "$BIN" ]]; then
  echo "error: no executable at $BIN" >&2
  echo "hint: cargo build --release  (or --bin /opt/hermit/bin/hermit)" >&2
  exit 1
fi
if [[ ! -f "$CONFIG" ]]; then
  echo "error: no config at $CONFIG" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Canned requests: a deliberate mix so each gate gets samples.
#   - no-tool chat        -> text TTFT gate
#   - lookups             -> one-search gate
#   - device commands     -> fast-path gate
if [[ -n "$PROMPTS" ]]; then
  cp "$PROMPTS" "$TMP/prompts.txt"
else
  cat > "$TMP/prompts.txt" <<'EOF'
what is the capital of Peru
explain in one sentence why the sky is blue
give me a two sentence summary of how a heat pump works
what is seven times eight
tell me a very short joke
pause
volume up
what time is it
next
stop
what is the weather in Oslo right now
what is the current price of copper
who won the last world cup
what is the latest news about the Nord Stream pipeline
how tall is the Burj Khalifa
what is the population of Iceland
when does the sun set in Bergen today
what is the exchange rate for the pound to the euro
volume 50
what is playing
EOF
fi

# Repeat the prompt list until we have $RUNS requests.
: > "$TMP/input.txt"
count=0
while [[ $count -lt $RUNS ]]; do
  while IFS= read -r line; do
    [[ $count -ge $RUNS ]] && break
    echo "$line" >> "$TMP/input.txt"
    count=$((count + 1))
  done < "$TMP/prompts.txt"
done
echo "/quit" >> "$TMP/input.txt"

echo "running $RUNS requests through $BIN ..." >&2
# stdout = answers, stderr = structured logs including hermit_timing lines.
"$BIN" run --config "$CONFIG" \
  < "$TMP/input.txt" \
  > "$TMP/answers.txt" \
  2> "$TMP/log.txt" || {
    echo "daemon exited non-zero; last 20 log lines:" >&2
    tail -20 "$TMP/log.txt" >&2
    exit 1
  }

grep -c 'hermit_timing' "$TMP/log.txt" > /dev/null || {
  echo "error: no hermit_timing lines found. Is HERMIT_LOG filtering them out?" >&2
  tail -20 "$TMP/log.txt" >&2
  exit 1
}

python3 - "$TMP/log.txt" <<'PY'
import re, sys, statistics as st

path = sys.argv[1]
rows = []
# tracing's fmt layer renders fields as key=value on the line.
kv = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|[^\s]+)')

for line in open(path, encoding="utf-8", errors="replace"):
    if "hermit_timing" not in line:
        continue
    d = {}
    for k, v in kv.findall(line):
        d[k] = v.strip('"')
    if "total_ms" in d:
        rows.append(d)

if not rows:
    print("no timing rows parsed", file=sys.stderr)
    sys.exit(1)

def nums(key, rows):
    out = []
    for r in rows:
        v = r.get(key, "-")
        if v in ("-", "", None):
            continue
        try:
            out.append(float(v))
        except ValueError:
            pass
    return out

def pct(v, p):
    if not v:
        return None
    v = sorted(v)
    i = min(len(v) - 1, int(round((p / 100.0) * (len(v) - 1))))
    return v[i]

def fmt(x):
    return "  -  " if x is None else f"{x:7.1f}"

fast = [r for r in rows if r.get("fast_path") == "true"]
agent = [r for r in rows if r.get("fast_path") != "true"]
notool = [r for r in agent if r.get("tool_rounds") in ("0", None)]
tooled = [r for r in agent if r.get("tool_rounds") not in ("0", None)]

print()
print(f"  requests: {len(rows)}   fast-path: {len(fast)}   no-tool: {len(notool)}   with-tools: {len(tooled)}")
print()
print(f"  {'stage':<28} {'p50':>8} {'p95':>8}   {'n':>4}")
print("  " + "-" * 54)

def row(label, values):
    print(f"  {label:<28} {fmt(pct(values,50))} {fmt(pct(values,95))}   {len(values):>4}")

row("route", nums("route_ms", rows))
row("memory recall", nums("recall_ms", rows))
row("prompt assemble", nums("assemble_ms", rows))
row("LOCAL OVERHEAD (gate 15)", nums("local_overhead_ms", rows))
print()
row("TTFT no-tool (gate 700)", nums("ttft_ms", notool))
row("TTFT with-tools", nums("ttft_ms", tooled))
row("TTS first audio", nums("tts_ttfa_ms", rows))
row("first audio no-tool (1200)", nums("first_audio_ms", notool))
row("first audio +search (2000)", nums("first_audio_ms", tooled))
print()
row("fast path total (gate 50)", nums("total_ms", fast))
row("total no-tool", nums("total_ms", notool))
row("total with-tools", nums("total_ms", tooled))

hits = sum(1 for r in rows if r.get("prefetch_hit") == "true")
fired = sum(1 for r in rows if r.get("prefetch_fired") == "true")
if fired:
    print(f"\n  speculative prefetch: {hits}/{fired} used ({100.0*hits/fired:.0f}%)")

print()
gates = [
    ("local overhead", pct(nums("local_overhead_ms", rows), 50), 15.0),
    ("text TTFT (no tools)", pct(nums("ttft_ms", notool), 50), 700.0),
    ("first audio (no tools)", pct(nums("first_audio_ms", notool), 50), 1200.0),
    ("first audio (one search)", pct(nums("first_audio_ms", tooled), 50), 2000.0),
    ("fast-path command", pct(nums("total_ms", fast), 50), 50.0),
]
failed = 0
for name, value, gate in gates:
    if value is None:
        print(f"  [ SKIP ] {name:<28} no samples")
        continue
    ok = value <= gate
    failed += 0 if ok else 1
    print(f"  [{' PASS ' if ok else ' FAIL '}] {name:<28} p50 {value:7.1f} ms   gate {gate:.0f} ms")

print()
sys.exit(1 if failed else 0)
PY
