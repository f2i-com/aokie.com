# Runtime secrets

Create three independent files in this directory. Files ending in `.txt` are
ignored by Git; the stack refuses the checked-in placeholders.

```sh
umask 077
openssl rand -hex 32 > admission-hmac.txt
openssl rand -hex 32 > lease-hmac.txt
openssl rand -hex 32 > turn-rest.txt
# The gateway reads its files as uid 65532 and coturn as uid 65534, and
# Docker mounts them with their host owner and mode.
sudo chown 65532:65532 admission-hmac.txt lease-hmac.txt
sudo chown 65534:65534 turn-rest.txt
sudo chmod 0400 admission-hmac.txt lease-hmac.txt turn-rest.txt
```

- `admission-hmac.txt` is shared only with the trusted FormLogic/custom
  admission issuer.
- `lease-hmac.txt` belongs only to the realtime gateway.
- `turn-rest.txt` is shared only with coturn and the trusted ICE credential
  issuer.

Do not put any of these values in discovery JSON, a Companion build, a Desktop
plugin manifest, Docker image layers, Compose environment variables, logs or
support bundles. Back them up in a secret manager. Rotation is a coordinated
operation; see the parent deployment README.
