#!/usr/bin/env bash
# The golden intent corpus: every text in target/corpus/heard.txt (fetch.sh) through the text rules of
# two builds, and the difference. Empty means the rules read every text the same way.
#   compare.sh [base ref, default origin/main] [other ref, default the working tree]
# A ref is checked out in a git worktree under target/corpus/ and gets this tree's
# examples/intent_corpus.rs, so a ref from before the tool still builds it. Both builds share target/.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD BASE=${1:-origin/main} OTHER=${2:-}
OUT=target/corpus
TOOL=crates/voice-assistant/examples/intent_corpus.rs
[ -s "$OUT/heard.txt" ] || { echo "no $OUT/heard.txt: run fetch.sh first"; exit 1; }

# Builds the tool in a tree and runs it on the corpus: build <tree> <name>.
build() {
  local tree=$1 name=$2
  if [ "$tree" != "$ROOT" ]; then cp "$TOOL" "$tree/$TOOL"; fi
  (cd "$tree" && CARGO_TARGET_DIR="$ROOT/target" cargo build -q -p voice-assistant --example intent_corpus)
  cp target/debug/examples/intent_corpus "$OUT/$name-intent_corpus"
  "$OUT/$name-intent_corpus" --catalogue "$OUT/heard.txt" > "$OUT/$name.txt"
}

# Checks out a ref in target/corpus/<name>: checkout <ref> <name>.
checkout() {
  local ref=$1 dir=$OUT/$2
  if [ -d "$dir" ]; then
    git -C "$dir" checkout -q --force --detach "$ref"
  else
    git worktree add -q --detach "$dir" "$ref"
  fi
}

checkout "$BASE" base
build "$ROOT/$OUT/base" base
if [ -n "$OTHER" ]; then
  checkout "$OTHER" other
  build "$ROOT/$OUT/other" other
else
  build "$ROOT" other
fi
texts=$(grep -vc '^  ' "$OUT/other.txt")
if diff -u "$OUT/base.txt" "$OUT/other.txt" > "$OUT/diff.txt"; then
  echo "same: $texts texts read alike by $BASE and ${OTHER:-the working tree}"
else
  echo "$(grep -c '^[-+]  ' "$OUT/diff.txt") lines differ in $texts texts; see $OUT/diff.txt"
  exit 1
fi
