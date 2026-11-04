#!/bin/sh
cd "$(dirname "$0")/.." || exit 1
pass=0; fail=0
check() { name=$1; shift; if out=$("$@" 2>&1); then pass=$((pass+1)); echo "PASS  $name"; else fail=$((fail+1)); echo "FAIL  $name"; echo "$out" | tail -15 | sed 's/^/      /'; fi; }

check "unit tests" cargo test -q
check "lint (clippy, warnings are errors)" cargo clippy -q -- -D warnings
check "release build" cargo build -q --release
size=$(wc -c < target/release/nexy-engine)
check "binary <= 5 MB ($size bytes)" test "$size" -le 5242880
check "memory: 10,000 idle bots within budget" ./target/release/nexy-engine idle-memory 10000
if [ -n "$NEXY_ENGINE_MODEL_URL" ]; then
  d=$(mktemp -d)
  check "backend probe ($NEXY_ENGINE_MODEL_URL)" env NEXY_ENGINE_DATA="$d" ./target/release/nexy-engine probe
  check "tool round trip (remember, then recall)" sh -c "env NEXY_ENGINE_DATA=$d ./target/release/nexy-engine ask --session t 'Remember that the test word is heliotrope.' >/dev/null && env NEXY_ENGINE_DATA=$d ./target/release/nexy-engine ask --session t2 'What is the test word? Check your saved facts.' | grep -qi heliotrope"
  rm -rf "$d"
fi
echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
