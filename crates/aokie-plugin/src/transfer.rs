//! Transfer of a live call to the owner on the OAIY route: the `transfer_v1`
//! contract (`docs/contracts/transfer/`).
//!
//! OAIY's call agent asks for a transfer with the realtime tool
//! `transfer_to_owner`; the plugin rings the owner's accepting endpoints
//! through the assistance broker and the Companion gateway, and reports how it
//! ended with a `formlogic.realtime.transfer_outcome` frame. Nothing here
//! touches audio: the caller stays with the AI until an endpoint has won the
//! request and the existing v2 takeover path moves the caller.

/// The realtime tool the OAIY call agent calls.
pub const TOOL_NAME: &str = "transfer_to_owner";

/// The feature name OAIY lists in `ready.features` when it implements the
/// contract for a call whose start said `allowTransfer`.
pub const FEATURE: &str = "transfer_v1";
