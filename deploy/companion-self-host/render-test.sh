#!/bin/sh
# Audit AK-14 — acceptance test for render-traefik-config.sh.
#
# An email local part may legally contain `/` and `&`; both used to corrupt
# the sed-rendered Traefik config (delimiter break / matched-placeholder
# expansion). This exercises the EXACT production render script against temp
# dirs and asserts each rendered file:
#   - contains the email verbatim,
#   - has no placeholder left behind,
#   - parses as YAML (python3+PyYAML when available — always present on the
#     Linux CI runner; locally a loud NOTE is printed if the parser is
#     missing, the structural checks still gate).
#
# Run from anywhere:  sh deploy/companion-self-host/render-test.sh
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() {
  echo "render-test: FAIL — $*" >&2
  exit 1
}

yaml_parse() {
  # Returns 0 if parsed, 2 if no parser available.
  if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' 2>/dev/null; then
    python3 -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1], encoding="utf-8"))' "$1"
  elif command -v python >/dev/null 2>&1 && python -c 'import yaml' 2>/dev/null; then
    python -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1], encoding="utf-8"))' "$1"
  else
    return 2
  fi
}

parser_missing=0
# The audit's two adversarial addresses plus a plain control.
for email in 'ops/turn@example.com' 'a&b@example.com' 'admin@example.com'; do
  tdir="$work/t" && rdir="$work/r"
  rm -rf "$tdir" "$rdir" && mkdir -p "$tdir" "$rdir"
  cp "$here/traefik-static.template.yml" "$here/traefik-dynamic.template.yml" "$tdir/"

  SIGNAL_HOST='signal.example.com' TURN_HOST='turn.example.com' ACME_EMAIL="$email" \
    TEMPLATE_DIR="$tdir" RENDER_DIR="$rdir" sh "$here/render-traefik-config.sh" \
    || fail "render script exited nonzero for ACME_EMAIL='$email'"

  for f in "$rdir/traefik.yml" "$rdir/dynamic.yml"; do
    [ -s "$f" ] || fail "$f missing/empty for '$email'"
    if grep -q '__ACME_EMAIL__\|__SIGNAL_HOST__\|__TURN_HOST__' "$f"; then
      fail "unrendered placeholder left in $f for '$email'"
    fi
  done
  # The email lands in the static config; it must appear VERBATIM exactly as
  # many times as the template carried the placeholder.
  want=$(grep -c '__ACME_EMAIL__' "$tdir/traefik-static.template.yml")
  got=$(grep -cF "$email" "$rdir/traefik.yml") || true
  [ "$got" = "$want" ] || fail "expected '$email' verbatim x$want in traefik.yml, found x$got"
  grep -qF 'signal.example.com' "$rdir/dynamic.yml" || fail "SIGNAL_HOST not rendered for '$email'"

  for f in "$rdir/traefik.yml" "$rdir/dynamic.yml"; do
    if yaml_parse "$f"; then :; else
      status=$?
      if [ "$status" = 2 ]; then parser_missing=1
      else fail "$f is not valid YAML for '$email'"
      fi
    fi
  done
  echo "render-test: OK — '$email'"
done

if [ "$parser_missing" = 1 ]; then
  echo "render-test: NOTE — no python YAML parser found locally; structural checks passed but the YAML-validity assertion did NOT run (CI's Linux runner always runs it)." >&2
fi
echo "render-test: PASS"
