# Production WinUSB package

`aokie_winusb_bluetooth.inf` is the immutable, all-supported-HWID package
submitted to Microsoft Hardware Dev Center. A public release additionally
requires the returned Microsoft attestation/WHQL-signed file named exactly:

`aokie_winusb_bluetooth.cat`

The catalog is intentionally not represented by a placeholder. Release CI
fails if it is missing, if kernel-policy signature verification fails, or if
the catalog does not bind the exact INF bytes. After changing the INF, obtain
a new catalog before attempting another release.

The release build pins both file digests into the elevated helper, copies the
same pair into `driver-package/`, includes their digests in the recursively
signed package manifest, and refuses installation if any byte differs.

The separate `managed-beta-driver` build flavour does not weaken this public
release path. When explicitly enabled at compile time and opted into at
runtime, it renders a single selected external dongle's INF inside trusted
code and locally signs the generated catalog. It must be labelled managed beta,
not distributed as the normal production package.
