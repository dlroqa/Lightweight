#!/usr/bin/env bash
# Capture the headers of real models for the lightweight-gguf integration tests.
#
# Only the first few megabytes of each file are fetched, with an HTTP range
# request, because everything we parse lives in the header. The full models run
# to tens of gigabytes and none of it would be read.
#
# The captures are not committed; run this to enable the tests that use them.
set -euo pipefail
cd "$(dirname "$0")/.."
dest=fixtures/real-headers
mkdir -p "$dest"

# repo | file | local name. One per architecture named in spec section 5.
models=(
  "LiquidAI/LFM2-1.2B-GGUF|LFM2-1.2B-Q4_K_M-hip-optimized.gguf|lfm2"
  "Qwen/Qwen3-1.7B-GGUF|Qwen3-1.7B-Q8_0.gguf|qwen3"
  "bartowski/Llama-3.2-3B-Instruct-GGUF|Llama-3.2-3B-Instruct-Q4_K_M.gguf|llama32"
  "unsloth/gemma-3-1b-it-GGUF|gemma-3-1b-it-Q4_K_M.gguf|gemma3"
  "ggml-org/SmolLM3-3B-GGUF|SmolLM3-Q4_K_M.gguf|smollm3"
  "unsloth/Phi-4-mini-instruct-GGUF|Phi-4-mini-instruct-Q4_K_M.gguf|phi4mini"
)

for entry in "${models[@]}"; do
  IFS='|' read -r repo file name <<< "$entry"
  out="$dest/$name.gguf"
  if [ -s "$out" ]; then echo "  cached  $name"; continue; fi
  curl -sSL --max-time 180 -H "Range: bytes=0-8388607" \
    -o "$out" "https://huggingface.co/$repo/resolve/main/$file"
  # `wc -c` rather than `stat -c%s`: the latter is GNU-only, so on macOS this
  # line failed, the script exited non-zero, and `check.sh` then reported the
  # tier as "headers unavailable (offline?)" - a skip message naming the wrong
  # reason, which the skip-announcement convention exists to prevent.
  printf '  fetched %-10s %s bytes\n' "$name" "$(wc -c < "$out" | tr -d ' ')"
done
