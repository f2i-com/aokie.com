#!/usr/bin/env python3
"""Offline reference for Aokie admission and coturn REST credentials.

This is intentionally not an HTTP auth service. A real FormLogic/custom issuer
must authenticate the caller, authorize the exact app/device/role/scopes, rate
limit issuance and audit it before applying these deterministic encodings.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import re
import secrets
import time
from pathlib import Path


SAFE_ID = re.compile(r"^[A-Za-z0-9_.:-]{1,200}$")
SCOPES = {
    "state_read",
    "caller_read",
    "captions_read",
    "assistance_read",
    "assistance_respond",
    "monitor",
    "consult",
    "takeover",
    "resume_aokie",
    "rtc_signal",
}
MAX_SAFE_INTEGER = (1 << 53) - 1
MAX_APPROVED_PEERS = 64


def read_secret(path: str) -> bytes:
    value = Path(path).read_bytes().rstrip(b"\r\n")
    if not 32 <= len(value) <= 4096:
        raise ValueError(f"{path}: secret must contain 32..4096 bytes")
    if b"REPLACE" in value or b"CHANGE_ME" in value:
        raise ValueError(f"{path}: refusing placeholder secret")
    return value


def safe_id(value: str, label: str) -> str:
    if not SAFE_ID.fullmatch(value):
        raise ValueError(f"{label} must match {SAFE_ID.pattern}")
    return value


def peer_roster_hash(revision: int, thumbprints: list[str]) -> str:
    payload = json.dumps(
        {
            "approvedPeerKeyThumbprints": sorted(thumbprints),
            "peerRosterRevision": revision,
        },
        separators=(",", ":"),
        sort_keys=True,
        ensure_ascii=True,
    ).encode("ascii")
    digest = hashlib.sha256(b"aokie/v2/peer-roster\0" + payload).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")


def admission_peer_policy(args: argparse.Namespace) -> dict:
    holder = safe_id(args.holder_key_thumbprint, "holderKeyThumbprint")
    expected_peer = args.expected_peer_key_thumbprint
    approved_peers = args.approved_peer_key_thumbprint
    roster_revision = args.peer_roster_revision

    if args.role == "mobile":
        if expected_peer is None:
            raise ValueError("mobile admissions require --expected-peer-key-thumbprint")
        expected_peer = safe_id(expected_peer, "expectedPeerKeyThumbprint")
        if expected_peer == holder:
            raise ValueError("mobile holder and expected peer thumbprints must be different")
        if approved_peers or roster_revision is not None:
            raise ValueError(
                "mobile admissions must not specify a plugin approved-peer roster"
            )
        return {
            "holderKeyThumbprint": holder,
            "expectedPeerKeyThumbprint": expected_peer,
        }

    if args.role != "plugin":
        raise ValueError("role must be mobile or plugin")
    if expected_peer is not None:
        raise ValueError(
            "plugin admissions must not specify --expected-peer-key-thumbprint"
        )
    if not approved_peers:
        raise ValueError(
            "plugin admissions require at least one --approved-peer-key-thumbprint"
        )
    if len(approved_peers) > MAX_APPROVED_PEERS:
        raise ValueError(f"plugin admissions allow at most {MAX_APPROVED_PEERS} peers")
    peers = sorted(
        safe_id(thumbprint, "approvedPeerKeyThumbprints")
        for thumbprint in approved_peers
    )
    if len(set(peers)) != len(peers):
        raise ValueError("plugin approved-peer thumbprints must be unique")
    if holder in peers:
        raise ValueError("plugin holder must not appear in its approved-peer roster")
    if roster_revision is None or not 1 <= roster_revision <= MAX_SAFE_INTEGER:
        raise ValueError(
            "plugin admissions require --peer-roster-revision in the safe integer range"
        )
    return {
        "holderKeyThumbprint": holder,
        "approvedPeerKeyThumbprints": peers,
        "peerRosterRevision": roster_revision,
        "peerRosterHash": peer_roster_hash(roster_revision, peers),
    }


def mint_admission(secret: bytes, args: argparse.Namespace, now: int) -> tuple[str, dict]:
    if not 1 <= args.admission_ttl <= 300:
        raise ValueError("admission TTL must be 1..300 seconds")
    scopes = list(dict.fromkeys(args.scope))
    unknown = set(scopes) - SCOPES
    if unknown:
        raise ValueError(f"unknown scopes: {', '.join(sorted(unknown))}")
    claims = {
        "aud": "aokie-v2-gateway",
        "appId": safe_id(args.app_id, "appId"),
        "subjectId": safe_id(args.subject_id, "subjectId"),
        "role": args.role,
        **admission_peer_policy(args),
        "scopes": scopes,
        "exp": now + args.admission_ttl,
        "jti": safe_id("adm_" + secrets.token_hex(16), "jti"),
    }
    payload = json.dumps(claims, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    signature = hmac.new(secret, payload, hashlib.sha256).hexdigest()
    return f"aokie-adm-v2.{payload.hex()}.{signature}", claims


def mint_turn(secret: bytes, args: argparse.Namespace, now: int) -> dict:
    if not 60 <= args.turn_ttl <= 3600:
        raise ValueError("TURN TTL must be 60..3600 seconds")
    expiry = now + args.turn_ttl
    username = f"{expiry}:{args.subject_id}"
    credential = base64.b64encode(hmac.new(secret, username.encode(), hashlib.sha1).digest()).decode()
    return {
        "username": username,
        "credential": credential,
        "expiresAt": expiry,
        "urls": [
            f"stun:{args.turn_host}:3478",
            f"turn:{args.turn_host}:3478?transport=udp",
            f"turn:{args.turn_host}:3478?transport=tcp",
            f"turns:{args.turn_host}:5349?transport=tcp",
        ],
        "iceTransportPolicy": "relay" if args.relay_only else "all",
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--admission-secret-file", required=True)
    result.add_argument("--turn-secret-file", required=True)
    result.add_argument("--gateway-url", required=True)
    result.add_argument("--turn-host", required=True)
    result.add_argument("--app-id", required=True)
    result.add_argument("--subject-id", required=True)
    result.add_argument("--role", choices=("mobile", "plugin"), required=True)
    result.add_argument("--holder-key-thumbprint", required=True)
    result.add_argument("--expected-peer-key-thumbprint")
    result.add_argument("--approved-peer-key-thumbprint", action="append", default=[])
    result.add_argument("--peer-roster-revision", type=int)
    result.add_argument("--scope", action="append", default=[])
    result.add_argument("--admission-ttl", type=int, default=120)
    result.add_argument("--turn-ttl", type=int, default=600)
    result.add_argument("--relay-only", action="store_true")
    return result


def main() -> None:
    args = parser().parse_args()
    if not args.gateway_url.startswith(("wss://", "ws://127.0.0.1:", "ws://localhost:")):
        raise ValueError("gateway URL must be public wss:// or an exact loopback development ws:// URL")
    safe_id(args.subject_id, "subjectId")
    now = int(time.time())
    token, claims = mint_admission(read_secret(args.admission_secret_file), args, now)
    output = {
        "gatewayUrl": args.gateway_url,
        "admissionToken": token,
        "admissionClaims": claims,
        "admissionExpiresAt": claims["exp"],
        "ice": mint_turn(read_secret(args.turn_secret_file), args, now),
    }
    print(json.dumps(output, separators=(",", ":")))


if __name__ == "__main__":
    main()
