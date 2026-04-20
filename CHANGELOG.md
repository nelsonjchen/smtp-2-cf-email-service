# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-19

### Added

- Initial Rust SMTP submission relay that forwards mail to Cloudflare Email Service using the raw MIME API.
- Durable on-disk spool queue with retries, backoff, and dead-letter handling.
- SMTP AUTH support for a single configured account, including Gmail-compatible `AUTH PLAIN` and `AUTH LOGIN`.
- TLS discovery from Certbot paths and listener support for port `25`, `587`, and `465` when certificate material is available.
- Self-restart when watched TLS certificate files change.
- Example `systemd` unit, sample config, public-safe agent guidance, and MIT licensing.
- GitHub Actions workflows for CI and release-style Linux binary artifacts.

### Changed

- Switched the SMTP listener to the `smtpd` crate for the production path.
- Sanitized transport-generated headers before Cloudflare `send_raw` so Gmail-shaped MIME is accepted reliably.
- Standardized deployment guidance around local Docker-based Linux `amd64` builds instead of compiling on the VPS.

### Security

- Removed private infrastructure details and personal identifiers from the tracked repo and rewritten Git history before public release.

[Unreleased]: https://github.com/nelsonjchen/smtp-2-cf-email-service/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nelsonjchen/smtp-2-cf-email-service/releases/tag/v0.1.0
