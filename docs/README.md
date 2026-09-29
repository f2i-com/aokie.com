# Aokie documentation

The guides for the Aokie Phone Bridge. Start with the [repository README](../README.md)
for what Aokie is, how to build it and how to install it into OAIY.

## Guides

| Guide | What it covers |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | The plugin process and its host, the call-session state machine, Aokie's own voice pipeline, the OAIY route and durable event delivery. |
| [FORMLOGIC_PLUGIN_CONTRACT.md](FORMLOGIC_PLUGIN_CONTRACT.md) | What the plugin owes its host: the schemaVersion 4 manifest and setup wizard, commands and events, setup mode, the OAIY route, call waiting, voice and pairing rules. |
| [HARDWARE.md](HARDWARE.md) | Supported USB Bluetooth dongles and their tiers, phones, Windows and Linux notes, and how to add a dongle. |
| [CONVERSATION_TIMING.md](CONVERSATION_TIMING.md) | Turn-taking settings for calls Aokie's own speech answers, and how dates and times are spoken. |
| [MODEL_LICENSES.md](MODEL_LICENSES.md) | Licences of the speech models Aokie's installer downloads. |
| [dependency-maintenance.md](dependency-maintenance.md) | Dependencies that must be upgraded together, and the manual release checks. |
| [NATIVE_BLUETOOTH_TRANSPORT_PLAN.md](NATIVE_BLUETOOTH_TRANSPORT_PLAN.md) | **Done; recommendation superseded.** The plan and field findings for the native Windows Bluetooth backend (`aokie-winbt`), which cannot carry call audio on Windows 11 25H2. |

## Contracts and data

| Path | What it holds |
|---|---|
| [contracts/](contracts/) | The JSON schemas and shared contracts: the plugin manifest, the desktop event envelope, connector requests and responses, the connector contract, the settings schema, the default persona, and the Companion realtime protocols (v1 and v2). The shared files are byte-identical with FormLogic's copies and locked by tests in both repos, so change them in both at once. |
| [contracts/fixtures/](contracts/fixtures/) | Companion protocol messages that the Rust tests (`aokie-protocol`, `aokie-realtime`, Companion) check against the schemas. |
| [contracts/transfer/](contracts/transfer/) | `transfer_v1`, transferring a live call to the owner on the OAIY route: the contract document and the JSON fixtures (the tool call and result, the outcome frame, start and ready, the ring plan, the reserved offer id, the caller-asked phrase check). The fixtures are shared byte for byte with the OAIY repository; `SHA256SUMS` and `scripts/check-contracts.mjs` keep the two copies in step. |
| [models-manifest.json](models-manifest.json) | The speech model revisions, file sizes and SHA-256 digests the installer accepts. |
| [readme-assets/](readme-assets/) | The README's screenshots, and how they were taken. |

## Elsewhere in the repository

| Guide | What it covers |
|---|---|
| [SECURITY.md](../SECURITY.md) | Reporting a vulnerability, supported versions, security boundaries and release bundle signing. |
| [apps/aokie-mobile/README.md](../apps/aokie-mobile/README.md) | Aokie Companion, the phone and desktop app that listens to or takes over a call. |
| [apps/aokie-mobile/docs/NATIVE_RUNTIME.md](../apps/aokie-mobile/docs/NATIVE_RUNTIME.md) | Companion's native credential, wake, call and permission boundaries on each platform. |
| [crates/aokie-realtime/README.md](../crates/aokie-realtime/README.md) | The self-hostable Companion signalling gateway. |
| [deploy/companion-self-host/README.md](../deploy/companion-self-host/README.md) | Running the gateway and a TURN relay on your own server. |
| [drivers/winusb/README.md](../drivers/winusb/README.md) | The production WinUSB driver package and its Microsoft-signed catalog. |
| [vendor/README.md](../vendor/README.md) | The GLib 0.18 security backport patched into the workspace. |

## Other repositories

- FormLogic's side of the plugin contract, operations and troubleshooting:
  [f2i-com/formlogic.com](https://github.com/f2i-com/formlogic.com), in
  `docs/AOKIE_PLUGIN_CONTRACT.md`, `docs/AOKIE_OPERATIONS.md` and
  `docs/AOKIE_TROUBLESHOOTING.md`.
- OAIY's side: `docs/ecosystem/AOKIE_CONTRACT.md` in the OAIY repository.
