#!/usr/bin/env bash
# Keep credentials, and this machine, out of the repository.
#
# A tripwire, not a scanner. It cannot know whether a string is a real
# credential, so it flags the shapes that have no business being committed at
# all and lets everything else through:
#
#   * an IP address that is not loopback, not unspecified, and not from a range
#     reserved for documentation - this build runs on LANs and on several
#     overlay networks, and none of their addresses belong in the source.
#     A CIDR *range* (`10/8`, `fd00::/8`) is not an address and is allowed: it
#     names a standard, not a machine;
#   * a home directory path, which is one machine's filesystem;
#   * a bearer token or an assignment to a credential-shaped name.
#
# Addresses are supplied at runtime and printed to the operator's terminal.
# Nothing here should ever need to hard-code one.
set -uo pipefail
cd "$(dirname "$0")/.."

status=0
self="scripts/check-secrets.sh"

report() {
  echo "POLICY VIOLATION: $1"
  echo "$2" | sed 's/^/    /'
  echo
  status=1
}

# --- addresses ---------------------------------------------------------------
# Allowed, deliberately and exhaustively:
#   127.x, 0.0.0.0, ::1, ::            loopback and unspecified
#   192.0.2.x, 198.51.100.x, 203.0.113.x   RFC 5737 documentation ranges
#   2001:db8:...                       RFC 3849 documentation range
#   10.0.0.1, 100.64.0.1, fd00::1      range *base* addresses, used in tests to
#                                      prove every network is treated alike
allowed_v4='^(127\.|0\.0\.0\.0$|192\.0\.2\.|198\.51\.100\.|203\.0\.113\.|10\.0\.0\.1$|100\.64\.0\.1$)'

ipv4_hits=$(git grep -nIE '(^|[^0-9.])([0-9]{1,3}\.){3}[0-9]{1,3}([^0-9.]|$)' -- . \
  | grep -v "^$self:" \
  | while IFS= read -r line; do
      # Pull each dotted quad out of the line and test it on its own, so one
      # allowed address on a line does not excuse another beside it.
      echo "$line" | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}(/[0-9]{1,2})?' | while IFS= read -r addr; do
        # A CIDR range names a standard rather than a machine.
        case "$addr" in */*) continue ;; esac
        if ! echo "$addr" | grep -qE "$allowed_v4"; then
          echo "$line"
          break
        fi
      done
    done)
if [ -n "$ipv4_hits" ]; then
  report "an IPv4 address that is not loopback or documentation-range is committed." "$ipv4_hits"
fi

ipv6_hits=$(git grep -nIE '\b(f[cd][0-9a-f]{2}:[0-9a-f:]+|100\.6[4-9]\.)' -- . \
  | grep -v "^$self:" \
  | grep -vE 'fd00::1\b' \
  | grep -vE 'f[cd][0-9a-f]{2}:[0-9a-f:]*/[0-9]{1,3}')
if [ -n "$ipv6_hits" ]; then
  report "a unique-local IPv6 or shared-range address is committed." "$ipv6_hits"
fi

# --- home directories --------------------------------------------------------
home_hits=$(git grep -nIE '(/home/[a-z][-a-z0-9_]*/|/Users/[A-Za-z][-A-Za-z0-9_]*/|C:\\\\Users\\\\)' -- . \
  | grep -v "^$self:")
if [ -n "$home_hits" ]; then
  report "a home-directory path is committed; derive it at runtime instead." "$home_hits"
fi

# --- credentials -------------------------------------------------------------
# 16 characters is above the client's literal \`Bearer no-key-required\`, which is
# a documented placeholder rather than a secret.
bearer_hits=$(git grep -nIE 'Bearer [A-Za-z0-9._~+/-]{16,}' -- . | grep -v "^$self:")
if [ -n "$bearer_hits" ]; then
  report "a bearer token literal is committed." "$bearer_hits"
fi

assign_hits=$(git grep -nIE '(api_key|apikey|password|passwd|secret_key|access_token|auth_token|private_key)[[:space:]]*[:=][[:space:]]*["'"'"'][^"'"'"']{16,}' -- . \
  | grep -v "^$self:" | grep -vE '\$\{|\$\(|<[a-z-]+>|env!|std::env')
if [ -n "$assign_hits" ]; then
  report "a credential-shaped assignment with a literal value is committed." "$assign_hits"
fi

if [ "$status" -eq 0 ]; then
  echo "  ok  no credentials, home paths or machine addresses in tracked files"
fi
exit "$status"
