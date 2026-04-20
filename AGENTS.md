# AGENTS.md

## Deployment Preference

Before doing deployment work:

1. Check whether a local `.env` file exists in the repo root.
2. Check whether a local `AGENTS.local.md` file exists in the repo root.
3. If it exists, read it for private, machine-specific deployment instructions and operator notes.
4. Treat `.env` as the source of truth for local values and secrets.
5. Treat `AGENTS.local.md` as the place for private, prose-style operational guidance that should not live in Git.
6. Never commit `.env` or `AGENTS.local.md` contents back into the repository.

Preferred deployment path:

1. Build the release binary on the local Mac, not on the VPS.
2. Use a Linux `amd64` Docker container for the build so the artifact matches the server environment.
3. Copy only the built `smtp2cf` binary to the deployment host from `.env` or other local instructions.
4. Install it to `/usr/local/bin/smtp2cf`.
5. Restart the `smtp2cf` systemd service and verify it is healthy.

Reason:

- The VPS is very small and local containerized builds are much faster and more reliable than compiling on the server.
- Direct cross-compilation via `zigbuild` currently runs into `aws-lc-sys` archive issues, while the Docker-based Linux build works.

## Preferred Build Command

Run from the repo root on the Mac:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD":/work \
  -w /work \
  rust:1 \
  bash -lc 'export PATH=/usr/local/cargo/bin:$PATH; cargo build --release'
```

Resulting artifact:

- `target/release/smtp2cf`

Expected artifact type:

- Linux `x86_64` ELF executable suitable for the target server

## Preferred Deploy Command

```bash
scp target/release/smtp2cf "${DEPLOY_USER}@${DEPLOY_HOST}:/tmp/smtp2cf.new"
ssh "${DEPLOY_USER}@${DEPLOY_HOST}" '
  sudo install -m 0755 /tmp/smtp2cf.new /usr/local/bin/smtp2cf &&
  rm /tmp/smtp2cf.new &&
  sudo systemctl restart smtp2cf &&
  sudo systemctl is-active smtp2cf &&
  sudo journalctl -u smtp2cf -n 12 --no-pager
'
```

## Notes

- Prefer storing values like `DEPLOY_HOST` and `DEPLOY_USER` in the local `.env`.
- Prefer storing private deployment notes, hostnames, SSH shortcuts, and operator-specific runbooks in `AGENTS.local.md`.
- Do not put secrets in `AGENTS.local.md`; keep actual secret values in `.env`.
- Do not prefer VPS-native compilation unless local Docker builds are unavailable.
- If a deployment needs validation, send a smoke test through SMTPS on port `465` after restart.
- Confirm `message delivered to Cloudflare` in journald.
- If Gmail access is available, also confirm the smoke test arrives in the Gmail inbox and not only in Sent mail.
