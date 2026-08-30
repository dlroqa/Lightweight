#!/usr/bin/env bash
# Record the sha256 and size of every pinned model in lightweight-catalog::manifest.
#
# The digests come from HuggingFace's tree API, which reports each LFS object's
# own sha256 (`lfs.oid`) and size. No weights are downloaded: the response is a
# few kilobytes of JSON per repository.
#
# This exists for the same reason the engine digests came from the GitHub
# release API rather than from a person's memory: a digest that was typed in is
# a digest nobody checked. Run it, paste the literals it prints into
# crates/lightweight-catalog/src/manifest.rs, and the download path can verify a
# model against a value recorded before the download.
set -euo pipefail
cd "$(dirname "$0")/.."

manifest=crates/lightweight-catalog/src/manifest.rs

# Read the repo/file pairs straight out of the manifest, so this script cannot
# drift from the list it is meant to record.
python3 - "$manifest" <<'PY'
import json
import re
import sys
import urllib.request

manifest = open(sys.argv[1]).read()

entries = re.findall(
    r'id:\s*"([^"]+)".*?repo:\s*"([^"]+)".*?file:\s*"([^"]+)"',
    manifest,
    re.S,
)
if not entries:
    sys.exit("no entries found in " + sys.argv[1])

def tree(repo):
    url = f"https://huggingface.co/api/models/{repo}/tree/main?recursive=true"
    request = urllib.request.Request(url, headers={"User-Agent": "lightweight-gateway"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)

failures = 0
for model_id, repo, filename in entries:
    try:
        listing = tree(repo)
    except Exception as err:  # noqa: BLE001 - reported, not raised
        print(f"  FAIL  {model_id}: {repo}: {err}")
        failures += 1
        continue

    match = next((e for e in listing if e.get("path") == filename), None)
    if match is None:
        available = sorted(
            e["path"] for e in listing if e.get("path", "").endswith(".gguf")
        )
        print(f"  FAIL  {model_id}: {filename} is not in {repo}")
        for path in available[:12]:
            print(f"          available: {path}")
        failures += 1
        continue

    lfs = match.get("lfs") or {}
    digest = lfs.get("oid")
    size = lfs.get("size", match.get("size"))
    if not digest or not size:
        print(f"  FAIL  {model_id}: {filename} has no LFS digest (not an LFS object?)")
        failures += 1
        continue

    print(f"  {model_id}")
    print(f'        sha256: "{digest}",')
    print(f"        size: {size:_},")

sys.exit(1 if failures else 0)
PY
