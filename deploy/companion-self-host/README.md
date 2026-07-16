# Self-hosting Aokie Companion realtime and TURN

This bundle runs the vendor-neutral Aokie v2 signalling gateway behind a
maintained Traefik TLS edge and a locked-down coturn relay. FormLogic is one
possible identity/admission issuer; a custom service can implement the same
small contracts below.

## Media boundary

`aokie-realtime` carries authenticated call state, captions, lease decisions,
SDP and ICE signalling. It **does not receive, route, record or decrypt PCM,
RTP, SRTP, microphone audio or speaker audio**. Media is peer-to-peer WebRTC
between the Aokie Desktop/plugin endpoint and the Companion endpoint.

When direct ICE cannot connect, coturn relays encrypted WebRTC packets. Coturn
can observe transport metadata and encrypted packet bytes, but it does not
terminate DTLS-SRTP or decrypt the call. PHP, the web app, JSON RPC and Tauri
IPC remain outside the audio path.

```text
caller <-> phone <-> Bluetooth HFP/SCO <-> Aokie plugin/Desktop
                                                |
                                      WebRTC (direct or TURN)
                                                |
                                      Companion mic/speakers

FormLogic/custom auth -> short-lived admission + TURN credentials
Aokie endpoints <-> WSS signalling gateway (state/SDP/ICE only)
```

## Public deployment

Prerequisites:

- a maintained Linux Docker Engine with Compose v2;
- `SIGNAL_HOST` and `TURN_HOST` A/AAAA records pointing at the host;
- inbound TCP 80, 443, 3478 and 5349;
- inbound UDP 3478;
- inbound TCP and UDP for the configured relay range (49160-49200 by default),
  with one-to-one port mapping;
- working Docker IPv6 plus routed/firewalled IPv6 before setting
  `TURN_EXTERNAL_IPV6` or publishing an AAAA record.

Create configuration and three independent secrets:

```sh
cd deploy/companion-self-host
cp .env.example .env
${EDITOR:-vi} .env
cd secrets
umask 077
openssl rand -hex 32 > admission-hmac.txt
openssl rand -hex 32 > lease-hmac.txt
openssl rand -hex 32 > turn-rest.txt
chmod 600 *.txt
cd ..
```

The example IPs/domains are documentation values and will not produce a usable
deployment. Check config, build, then start:

```sh
docker compose --env-file .env config --quiet
docker compose --env-file .env build --pull gateway
docker compose --env-file .env up -d
docker compose --env-file .env ps
curl --fail "https://${SIGNAL_HOST}/version"
```

Expected `/version` invariants are `v2Enabled: true`,
`v2DynamicAdmission: true`, `v2StaticAdmissionFallback: false`,
`signallingOnly: true`, and `mediaBridge: false`. The gateway has no published
container port; only Traefik reaches its private HTTP listener. Public
signalling is WSS at `wss://$SIGNAL_HOST/v2/realtime`; the edge exposes neither
v1 realtime nor a generic command API.

Traefik obtains and renews separate ACME certificates for the signalling and
TURN hostnames. TCP/5349 terminates TURN/TLS at Traefik and forwards plain TURN
over the private network using PROXY protocol v2. UDP/TCP 3478 and the relay
range terminate directly at coturn so STUN source addresses and relay port
mapping stay correct. DTLS/5349 is intentionally not exposed; use UDP/TCP 3478
or TURN/TLS over TCP/5349.

### IPv6

The Compose network is dual-stack and the port declarations bind both
`0.0.0.0` and `::`. Before enabling IPv6, confirm the Docker daemon has IPv6
enabled, the configured ULA subnet does not collide, the host has a public
IPv6 address, and every relay port is routed without port translation. Set
`TURN_EXTERNAL_IPV6` only after those checks. If IPv6 is unavailable, remove
the IPv6 port declarations and AAAA records rather than advertising a broken
path.

## Loopback development without public DNS

The local file publishes only loopback `ws://127.0.0.1:18788/v2/realtime` and
TURN on `127.0.0.1:3478`. It has no ACME, public listener or TLS and is for
debug clients on the same computer only. It deliberately allows private and
loopback TURN peers so relay-only tests can run locally.

```sh
docker compose -f compose.local.yaml config --quiet
docker compose -f compose.local.yaml up -d --build
curl --fail http://127.0.0.1:18788/version
```

Release clients must use the public WSS profile. Never expose the local file's
ports beyond loopback or set `TURN_ALLOW_PRIVATE_PEERS=1` in public.

## Admission issuer contract

The trusted FormLogic/custom auth service reads the same bytes as
`secrets/admission-hmac.txt`, after it authenticates the requester and verifies
that the device/desktop is authorized for that app. It creates compact UTF-8
JSON with exact camel-case identity and peer-policy claims (shown formatted
below for readability). A mobile admission binds its endpoint holder to one
expected plugin:

```json
{
  "aud": "aokie-v2-gateway",
  "appId": "app_123",
  "subjectId": "device_456",
  "role": "mobile",
  "holderKeyThumbprint": "mobile_thumbprint_a",
  "expectedPeerKeyThumbprint": "plugin_thumbprint_a",
  "scopes": ["state_read", "monitor", "rtc_signal"],
  "exp": 1784160120,
  "jti": "adm_unique_mobile_value"
}
```

A plugin admission binds its holder to the complete approved mobile roster.
The thumbprints must be unique and sorted lexicographically before hashing and
encoding the claim:

```json
{
  "aud": "aokie-v2-gateway",
  "appId": "app_123",
  "subjectId": "aokie",
  "role": "plugin",
  "holderKeyThumbprint": "plugin_thumbprint_a",
  "approvedPeerKeyThumbprints": ["mobile_thumbprint_a", "mobile_thumbprint_b"],
  "peerRosterRevision": 7,
  "peerRosterHash": "tsKgPP1ruPCU23HfLYaUChe9jHYHCtubve77gnlfyDw",
  "scopes": ["state_read", "rtc_signal"],
  "exp": 1784160120,
  "jti": "adm_unique_plugin_value"
}
```

- `aud` is exactly `aokie-v2-gateway`;
- `role` is `mobile` or `plugin`;
- IDs are 1-200 ASCII letters/digits or `- _ . :`;
- `holderKeyThumbprint` is the authenticated endpoint's exact key thumbprint;
- a mobile requires exactly one distinct `expectedPeerKeyThumbprint` and no
  roster fields;
- a plugin requires a non-empty, sorted `approvedPeerKeyThumbprints` list that
  does not contain its own holder, a positive monotonic `peerRosterRevision`,
  and `peerRosterHash = base64url-no-pad(SHA-256("aokie/v2/peer-roster\0" +
  canonical JSON of the sorted roster and revision))`; it has no expected-peer
  field;
- `exp` is a Unix second no more than 300 seconds after issuance;
- `jti` is unpredictable and unique per connection attempt (a live token
  replay is rejected);
- scopes are unique and least-privilege: `state_read`, `caller_read`,
  `captions_read`, `assistance_read`, `assistance_respond`, `monitor`,
  `consult`, `takeover`, `resume_aokie`, `rtc_signal`.

Sign the exact JSON bytes with HMAC-SHA256. The bearer token is:

```text
aokie-adm-v2.<lowercase hex of JSON bytes>.<lowercase hex HMAC>
```

Clients send it as `Authorization: Bearer <token>` on the WSS upgrade. Never
put admissions in query strings, discovery documents or logs. A managed
FormLogic issuer should return the deployment's WSS URL and token from its
authenticated Companion-admission endpoint; a custom issuer can return the
same fields. Grant `consult` only when both deployed endpoints support it.

## Expiring TURN REST credentials

The trusted issuer also reads the same bytes as `secrets/turn-rest.txt`. After
authorization it chooses a short expiry (600 seconds is a useful default,
never beyond coturn's 3600-second maximum allocation lifetime) and creates:

```text
username   = <expiry Unix seconds>:<authorized device id>
credential = base64(HMAC-SHA1(turn REST secret, UTF-8 username))
```

Return those values only to the authorized endpoint with these ICE URLs:

```json
{
  "urls": [
    "stun:turn.example.com:3478",
    "turn:turn.example.com:3478?transport=udp",
    "turn:turn.example.com:3478?transport=tcp",
    "turns:turn.example.com:5349?transport=tcp"
  ],
  "username": "1784160600:device_456",
  "credential": "short-lived-derived-value",
  "expiresAt": 1784160600
}
```

Use WebRTC `iceTransportPolicy: "all"` for direct-first with TURN fallback, or
`"relay"` when an operator/user explicitly requires relay-only. Policy is a
client-side ICE choice; the signalling gateway never becomes a media relay.

`examples/mint_credentials.py` is an offline interoperability reference, not
an authorization server. It can prove a FormLogic/custom implementation uses
the expected bytes without exposing an HTTP credential mint:

```sh
python examples/mint_credentials.py \
  --admission-secret-file secrets/admission-hmac.txt \
  --turn-secret-file secrets/turn-rest.txt \
  --gateway-url wss://signal.example.com/v2/realtime \
  --turn-host turn.example.com \
  --app-id app_123 --subject-id device_456 --role mobile \
  --holder-key-thumbprint mobile_thumbprint_a \
  --expected-peer-key-thumbprint plugin_thumbprint_a \
  --scope state_read --scope monitor --scope rtc_signal
```

For the plugin side, provide the holder plus every approved mobile key. The
reference sorts the list and derives the roster hash; it refuses a missing,
duplicate or self-referential peer policy:

```sh
python examples/mint_credentials.py \
  --admission-secret-file secrets/admission-hmac.txt \
  --turn-secret-file secrets/turn-rest.txt \
  --gateway-url wss://signal.example.com/v2/realtime \
  --turn-host turn.example.com \
  --app-id app_123 --subject-id aokie --role plugin \
  --holder-key-thumbprint plugin_thumbprint_a \
  --approved-peer-key-thumbprint mobile_thumbprint_b \
  --approved-peer-key-thumbprint mobile_thumbprint_a \
  --peer-roster-revision 7 \
  --scope state_read --scope rtc_signal
```

The JSON output includes `admissionClaims` so an issuer implementation can
compare the exact holder/peer policy and roster hash as well as the encoded
token.

## Hardening and operations

- Keep all three secrets in a secret manager and outside images, Git, Compose
  environment, logs and backups with broad access.
- The admission and TURN REST secrets cross a trust boundary only to the
  authorized issuer. The lease secret never leaves the gateway.
- Static v2 admissions are forcibly disabled by the image entrypoint. coturn
  uses time-limited REST auth, per-user/total quotas, bandwidth limits,
  unauthorized-response rate limiting, no CLI/admin plane, and public-profile
  peer deny ranges that block access to internal networks.
- Rate-limit the issuer per user/device/app, record issuance metadata (never
  tokens/secrets), and revoke the underlying device authorization. Tickets
  naturally expire; close active sockets when immediate revocation is needed.
- Rotate one secret at a time during a maintenance window. This protocol has
  no key ID or overlap set: drain sockets/allocations, update the issuer and
  mounted secret atomically, then recreate the affected service. Expect old
  tickets to fail closed.
- Back up Traefik's `acme_data` volume securely. Do not back up ephemeral edge
  config or coturn state. Monitor container health/restarts, certificate
  expiry, allocation quota/bandwidth, UDP loss and relay-range exhaustion.
- Pin image updates intentionally. The Compose file pins the tested Traefik
  v3.7.7 and coturn 4.14.0-r0 multi-platform manifests; review upstream
  security releases, update the pins, then retest WSS plus UDP/TCP/TLS TURN.
