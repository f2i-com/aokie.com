#!/bin/sh
set -eu

read_secret() {
  variable="$1"
  file="$2"
  label="$3"

  if [ ! -r "$file" ]; then
    # Docker mounts a Compose file secret with its host owner and mode; this
    # container runs as uid 65532, so on Linux the file must belong to it.
    echo "aokie-realtime: $label secret is not readable at $file by uid $(id -u); chown it to $(id -u) with mode 0400 (see the README)" >&2
    exit 78
  fi

  # The secret is the file's text without its trailing line break, CR and LF
  # alike, as examples/mint_credentials.py reads it: $(...) drops trailing LFs
  # only, so a file saved with CRLF kept its CR here, and every admission an
  # issuer signed with the text alone was refused.
  cr="$(printf '\r')"
  value="$(cat "$file")"
  while :; do
    case "$value" in
      *"$cr") value="${value%"$cr"}" ;;
      *"
") value="${value%"
"}" ;;
      *) break ;;
    esac
  done
  length="${#value}"
  if [ "$length" -lt 32 ] || [ "$length" -gt 4096 ]; then
    echo "aokie-realtime: $label secret must contain 32..4096 bytes" >&2
    exit 78
  fi
  case "$value" in
    *REPLACE*|*CHANGE_ME*)
      echo "aokie-realtime: refusing placeholder $label secret" >&2
      exit 78
      ;;
  esac

  export "$variable=$value"
}

: "${AOKIE_GATEWAY_ADMISSION_HMAC_SECRET_FILE:=/run/secrets/admission_hmac}"
: "${AOKIE_GATEWAY_LEASE_HMAC_SECRET_FILE:=/run/secrets/lease_hmac}"

read_secret AOKIE_GATEWAY_ADMISSION_HMAC_SECRET "$AOKIE_GATEWAY_ADMISSION_HMAC_SECRET_FILE" admission-HMAC
read_secret AOKIE_GATEWAY_LEASE_HMAC_SECRET "$AOKIE_GATEWAY_LEASE_HMAC_SECRET_FILE" lease-HMAC

# Production never accepts the long-lived static admission registry. The two
# secrets above are used only for short-lived admission and media-lease tokens.
export AOKIE_GATEWAY_ADMISSIONS='[]'
export AOKIE_GATEWAY_V2_ALLOW_STATIC_ADMISSIONS='0'

exec /usr/local/bin/aokie-realtime
