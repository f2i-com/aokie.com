//! Gateway tuning constants: timers, budgets, TTLs.

#[allow(unused_imports)]
use super::*;

pub(super) const READ_TICK: Duration = Duration::from_millis(100);
pub(super) const SNAPSHOT_POLL: Duration = Duration::from_millis(250);
pub(super) const SNAPSHOT_REFRESH: Duration = Duration::from_secs(10);
pub(super) const PING_INTERVAL: Duration = Duration::from_secs(15);
pub(super) const PONG_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const ADMISSION_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Begin replacing an admission while this much of its already safety-bounded
/// lifetime remains. The current carrier stays authoritative throughout the
/// overlap, so broker and endpoint-challenge latency cannot stop lease
/// heartbeats from reaching the session loop.
pub(super) const ADMISSION_ROTATION_OVERLAP: Duration = Duration::from_secs(30);
/// A replacement carrier opens away from the authority loop, but it is still
/// bounded: an attempt that cannot prove its endpoint before this window must
/// yield to a retry while the predecessor is still inside its safe lifetime.
pub(super) const ADMISSION_TRANSPORT_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const ADMISSION_ROTATION_RETRY_DELAY: Duration = Duration::from_secs(1);
// plugin.init's compact bootstrap intentionally omits token expiry. Consume it
// once and rotate through the Desktop broker quickly rather than assuming the
// gateway's maximum admission lifetime.
pub(super) const DEFAULT_BOOTSTRAP_LIFETIME: Duration = Duration::from_secs(45);
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(20);
pub(super) const ACTIVE_LEASE_FALLBACK_TTL: u64 = 20;
pub(super) const MAX_USED_ENDPOINT_JTIS: usize = 4_096;
pub(super) const ADMISSION_SAFETY_MARGIN_SECONDS: u64 = 10;
/// Desktop's token for the hosted-relay carrier in `supportedTransports`.
/// It compares the string exactly, so this is a shared wire constant.
pub(super) const RELAY_TRANSPORT: &str = "relay";
pub(super) const MIN_TURN_CREDENTIAL_TTL_SECONDS: u64 = 30;
pub(super) const MAX_TURN_CREDENTIAL_TTL_SECONDS: u64 = 24 * 60 * 60;
