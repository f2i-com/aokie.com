#!/bin/sh
set -eu

fail() {
  echo "coturn: $*" >&2
  exit 78
}

is_uint() {
  case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac
}

: "${TURN_SECRET_FILE:=/run/secrets/turn_rest}"
: "${TURN_REALM:?TURN_REALM is required}"
: "${TURN_EXTERNAL_IPV4:?TURN_EXTERNAL_IPV4 is required}"
: "${TURN_EXTERNAL_IPV6:=}"
: "${TURN_MIN_PORT:=49160}"
: "${TURN_MAX_PORT:=49200}"
: "${TURN_USER_QUOTA:=8}"
: "${TURN_TOTAL_QUOTA:=200}"
: "${TURN_MAX_BPS:=2000000}"
: "${TURN_BPS_CAPACITY:=100000000}"
: "${TURN_ALLOW_PRIVATE_PEERS:=0}"

[ -r "$TURN_SECRET_FILE" ] || fail "REST secret is not readable at $TURN_SECRET_FILE"
turn_secret="$(cat "$TURN_SECRET_FILE")"
[ "${#turn_secret}" -ge 32 ] && [ "${#turn_secret}" -le 4096 ] \
  || fail "REST secret must contain 32..4096 bytes"
case "$turn_secret" in
  *[!A-Za-z0-9._~+/=-]*) fail "REST secret must be one line of base64/base64url/hex-safe text" ;;
  *REPLACE*|*CHANGE_ME*) fail "refusing placeholder REST secret" ;;
esac

printf '%s' "$TURN_REALM" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$' \
  || fail "TURN_REALM must be a DNS-style name"
printf '%s' "$TURN_EXTERNAL_IPV4" | grep -Eq '^[0-9]{1,3}(\.[0-9]{1,3}){3}$' \
  || fail "TURN_EXTERNAL_IPV4 must be an IPv4 literal"
if [ -n "$TURN_EXTERNAL_IPV6" ]; then
  printf '%s' "$TURN_EXTERNAL_IPV6" | grep -Eq '^[0-9A-Fa-f:]+$' \
    || fail "TURN_EXTERNAL_IPV6 must be an IPv6 literal"
fi

for value in "$TURN_MIN_PORT" "$TURN_MAX_PORT" "$TURN_USER_QUOTA" "$TURN_TOTAL_QUOTA" "$TURN_MAX_BPS" "$TURN_BPS_CAPACITY"; do
  is_uint "$value" || fail "numeric TURN settings must contain decimal digits only"
done
[ "$TURN_MIN_PORT" -ge 1024 ] && [ "$TURN_MAX_PORT" -le 65535 ] && [ "$TURN_MIN_PORT" -le "$TURN_MAX_PORT" ] \
  || fail "TURN relay port range must be ordered and within 1024..65535"

private_v4="$(hostname -i | tr ' ' '\n' | grep -E '^[0-9]+(\.[0-9]+){3}$' | head -n 1 || true)"
private_v6="$(hostname -i | tr ' ' '\n' | grep ':' | head -n 1 || true)"
[ -n "$private_v4" ] || fail "container has no IPv4 address"
if [ -n "$TURN_EXTERNAL_IPV6" ] && [ -z "$private_v6" ]; then
  fail "TURN_EXTERNAL_IPV6 was set but the container network has no IPv6 address"
fi

runtime=/tmp/turnserver.conf
umask 077
cp /etc/coturn/turnserver.base.conf "$runtime"
{
  printf 'realm=%s\n' "$TURN_REALM"
  printf 'server-name=%s\n' "$TURN_REALM"
  printf 'static-auth-secret=%s\n' "$turn_secret"
  printf 'min-port=%s\nmax-port=%s\n' "$TURN_MIN_PORT" "$TURN_MAX_PORT"
  printf 'user-quota=%s\ntotal-quota=%s\n' "$TURN_USER_QUOTA" "$TURN_TOTAL_QUOTA"
  printf 'max-bps=%s\nbps-capacity=%s\n' "$TURN_MAX_BPS" "$TURN_BPS_CAPACITY"
  printf 'listening-ip=0.0.0.0\nrelay-ip=%s\n' "$private_v4"
  printf 'external-ip=%s/%s\n' "$TURN_EXTERNAL_IPV4" "$private_v4"
  if [ -n "$private_v6" ]; then
    printf 'listening-ip=::\nrelay-ip=%s\n' "$private_v6"
  fi
  if [ -n "$TURN_EXTERNAL_IPV6" ]; then
    printf 'external-ip=%s/%s\n' "$TURN_EXTERNAL_IPV6" "$private_v6"
  fi

  if [ "$TURN_ALLOW_PRIVATE_PEERS" = 1 ]; then
    # Development-only: allows a relay candidate to reach another endpoint on
    # the same workstation/LAN. The public profile never enables this.
    printf 'allow-loopback-peers\n'
  else
    # Prevent valid TURN users from using this server to reach loopback,
    # link-local, RFC1918/CGNAT, documentation, multicast or ULA networks.
    printf '%s\n' \
      'denied-peer-ip=0.0.0.0-0.255.255.255' \
      'denied-peer-ip=10.0.0.0-10.255.255.255' \
      'denied-peer-ip=100.64.0.0-100.127.255.255' \
      'denied-peer-ip=127.0.0.0-127.255.255.255' \
      'denied-peer-ip=169.254.0.0-169.254.255.255' \
      'denied-peer-ip=172.16.0.0-172.31.255.255' \
      'denied-peer-ip=192.0.0.0-192.0.0.255' \
      'denied-peer-ip=192.0.2.0-192.0.2.255' \
      'denied-peer-ip=192.168.0.0-192.168.255.255' \
      'denied-peer-ip=198.18.0.0-198.19.255.255' \
      'denied-peer-ip=198.51.100.0-198.51.100.255' \
      'denied-peer-ip=203.0.113.0-203.0.113.255' \
      'denied-peer-ip=224.0.0.0-255.255.255.255' \
      'denied-peer-ip=::-::ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff' \
      'denied-peer-ip=100::-1ff:ffff:ffff:ffff:ffff:ffff:ffff:ffff' \
      'denied-peer-ip=2001:db8::-2001:db8:ffff:ffff:ffff:ffff:ffff:ffff' \
      'denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff' \
      'denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff' \
      'denied-peer-ip=ff00::-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff'
  fi
} >> "$runtime"

unset turn_secret
exec turnserver -c "$runtime"
