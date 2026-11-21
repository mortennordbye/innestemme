#!/usr/bin/env bash
# Writes the reference transcript of each utterance in target/testset/utterances that has none yet,
# with Whisper large-v3-turbo on the Mac's GPU, to target/testset/reference.tsv (`<file>\t<ms>\t<text>`,
# what examples/stt_score.rs reads). Run sync.sh first.
set -euo pipefail
cd "$(dirname "$0")/../.."
DIR=target/testset; REF=$DIR/reference.tsv; touch "$REF"
TODO=$(comm -23 <(find "$DIR/utterances" -name "*.wav" -exec basename {} \; | sort) <(cut -f1 "$REF" | sort))
if [ -z "$TODO" ]; then echo "all labelled"; exit 0; fi
# shellcheck disable=SC2046,SC2086
WHISPER_SIZE=large-v3-turbo GPU=1 cargo run --release -q -p voice-assistant --features metal --example stt_eval -- \
  $(printf "$DIR/utterances/%s\n" $TODO) >> "$REF"
echo "$(echo "$TODO" | wc -l | tr -d ' ') labelled; $(wc -l < "$REF" | tr -d ' ') in $REF"
