#!/bin/sh
set -eu

fail() {
  echo "edge-config: $*" >&2
  exit 78
}

: "${SIGNAL_HOST:?SIGNAL_HOST is required}"
: "${TURN_HOST:?TURN_HOST is required}"
: "${ACME_EMAIL:?ACME_EMAIL is required}"

for host in "$SIGNAL_HOST" "$TURN_HOST"; do
  printf '%s' "$host" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$' \
    || fail "hostnames must be DNS names without a scheme, port, wildcard or path"
done
printf '%s' "$ACME_EMAIL" | grep -Eq '^[A-Za-z0-9.!#$%&*+/=?^_`{|}~-]+@[A-Za-z0-9.-]+$' \
  || fail "ACME_EMAIL is not a safe email address"

umask 077
# Audit AK-14: an email local part may legally contain `/` and `&` — both are
# sed metacharacters in an s/// replacement (`/` breaks the delimiter, `&`
# expands to the matched placeholder). Splice with awk index/substr instead,
# which has NO replacement metacharacters. The validated charset above has no
# backslash, so awk -v's escape processing cannot alter the value. Hostnames
# stay on sed: their validated charset ([A-Za-z0-9.-]) is sed-inert.
awk -v repl="$ACME_EMAIL" '{
  while ((i = index($0, "__ACME_EMAIL__")) > 0) {
    $0 = substr($0, 1, i - 1) repl substr($0, i + 14)
  }
  print
}' /templates/traefik-static.template.yml > /rendered/traefik.yml
sed -e "s/__SIGNAL_HOST__/$SIGNAL_HOST/g" -e "s/__TURN_HOST__/$TURN_HOST/g" \
  /templates/traefik-dynamic.template.yml > /rendered/dynamic.yml
chmod 0600 /rendered/traefik.yml /rendered/dynamic.yml
