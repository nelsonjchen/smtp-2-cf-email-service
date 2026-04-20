# smtp2cf

Small Rust SMTP submission relay for low-RAM VPSes. It accepts authenticated SMTP, durably spools raw MIME messages to disk, and forwards them to [Cloudflare Email Service](https://developers.cloudflare.com/email-service/get-started/send-emails/) using the raw MIME API.

Release history lives in [`CHANGELOG.md`](./CHANGELOG.md).

This project is designed for:

- Gmail "Send mail as" and similar SMTP clients
- cheap Ubuntu 20 VPSes
- one static SMTP username/password
- Certbot-managed TLS when available
- low operational complexity

## What It Does

- Listens on port `25` at all times
- Enables `587` with STARTTLS and `465` with implicit TLS when a Certbot certificate is found
- Does not advertise or allow SMTP AUTH on plaintext connections unless `allow_insecure_auth = true`
- Requires SMTP AUTH before accepting mail
- Restricts `MAIL FROM` and header `From` domains to configured allowed domains
- Stores accepted mail on disk before returning `250`
- Retries Cloudflare delivery on network errors, `429`, and `5xx`
- Dead-letters permanent failures instead of retrying forever

## Architecture

1. SMTP client connects to `smtp2cf`
2. Client authenticates with one configured username and password
3. Message is validated and written to `spool/pending/*.json`
4. SMTP client gets a `250` response only after the spool write is durable
5. Background worker sends the raw MIME message to Cloudflare `send_raw`
6. Success deletes the spool file; transient failures back off; permanent failures move to `spool/dead`

## Cloudflare API Shape

This service targets Cloudflare Email Service as documented on April 16, 2026:

- Guide: [Send emails](https://developers.cloudflare.com/email-service/get-started/send-emails/)
- Raw MIME endpoint: [send_raw](https://developers.cloudflare.com/api/resources/email_sending/methods/send_raw)

We use:

- `POST /accounts/{account_id}/email/sending/send_raw`
- `Authorization: Bearer ...`
- request body with `from`, `recipients`, and `mime_message`

## Configuration

Non-secret config lives in `config.toml`. A complete example is in [`config.example.toml`](./config.example.toml).

Secrets come from the environment:

- `SMTP2CF_SMTP_PASSWORD`
- `SMTP2CF_CLOUDFLARE_API_TOKEN`

Example:

```toml
hostname = "smtp.example.com"

[ports]
smtp_25 = "0.0.0.0:25"
submission_587 = "0.0.0.0:587"
smtps_465 = "0.0.0.0:465"

[auth]
username = "relay@example.com"

[senders]
allowed_domains = ["example.com", "mail.example.com"]

[cloudflare]
account_id = "your-account-id"

[tls]
cert_name = "mail.example.com"
allow_insecure_auth = false

[queue]
spool_dir = "/var/lib/smtp2cf/spool"
max_attempts = 10
backoff_initial_secs = 30
backoff_max_secs = 3600
poll_interval_secs = 5
```

## TLS Discovery

If `tls.cert_name` is set, the service looks for:

- `/etc/letsencrypt/live/<cert_name>/fullchain.pem`
- `/etc/letsencrypt/live/<cert_name>/privkey.pem`

You can override those with explicit paths in the config file.

Behavior:

- Cert found: `25`, `587`, and `465` are enabled with TLS support
- No cert found: only `25` starts, and AUTH stays unavailable unless insecure auth is explicitly enabled

## Gmail "Send Mail As"

Recommended Gmail settings:

- SMTP server: your VPS hostname
- Username: the configured `auth.username`
- Password: the value in `SMTP2CF_SMTP_PASSWORD`
- Port: `465` with SSL, or `587` with TLS

Do not use port `25` for Gmail unless you know exactly why.

## CLI

Validate config:

```bash
cargo run -- check-config --config ./config.example.toml
```

Run the service:

```bash
export SMTP2CF_SMTP_PASSWORD='replace-me'
export SMTP2CF_CLOUDFLARE_API_TOKEN='replace-me'

cargo run -- serve --config ./config.example.toml
```

## Build For Deployment

Prefer building on a local machine inside a Linux `amd64` Docker container, then copying only the resulting binary to the server:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD":/work \
  -w /work \
  rust:1 \
  bash -lc 'export PATH=/usr/local/cargo/bin:$PATH; cargo build --release'
```

This avoids slow builds on tiny VPSes and produces a Linux binary in an environment closer to the deployment target.

## systemd

An example unit lives at [`systemd/smtp2cf.service`](./systemd/smtp2cf.service).

Suggested layout on the server:

- binary: `/usr/local/bin/smtp2cf`
- config: `/etc/smtp2cf/config.toml`
- env file: `/etc/smtp2cf/smtp2cf.env`
- spool dir: `/var/lib/smtp2cf/spool`

The sample unit uses:

- `AmbientCapabilities=CAP_NET_BIND_SERVICE`
- restart-on-failure
- journald logging
- dedicated state/config directories

## Acceptance Scenarios

### First boot with Certbot certs present

- `check-config` reports TLS material found
- listeners start on `25`, `587`, and `465`
- Gmail can authenticate and send through `465` or `587`
- the process polls the active cert/key files and restarts itself when Certbot replaces them

### First boot with no certs present

- `check-config` reports that only port `25` will start
- AUTH is not advertised on plaintext unless insecure auth is enabled
- delivery still works only if you intentionally opt into insecure auth

### Service restart with queued mail pending

- spool files remain on disk
- worker resumes scanning `spool/pending`
- messages are retried according to the configured backoff

## Current Scope

This is a submission relay, not a full public MX:

- no inbound filtering pipeline
- no DKIM signing
- no zero-downtime live cert swap; the process restarts itself when cert files change
- no Docker packaging
- no account database

That keeps the service small, auditable, and cheap to run.
