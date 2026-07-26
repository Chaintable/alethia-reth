# Security Policy

This repository is a Chaintable-maintained downstream of
[taikoxyz/alethia-reth](https://github.com/taikoxyz/alethia-reth). It adds the
`trace_debankBlock` replay endpoint, frozen verification fixtures, and
Chaintable image publishing.

First determine whether the issue also reproduces on an unmodified upstream
`alethia-reth` build:

- Report upstream consensus, P2P, EVM, transaction-pool, storage, or standard
  RPC vulnerabilities to the upstream project through its private security
  reporting process.
- Report issues specific to this repository through the process below. This
  includes `trace_debankBlock`, its block-file or state-diff output, replay
  resource exhaustion, the published image, and CI.

## Supported Versions

Security updates are provided for the latest `main` branch and the latest
Chaintable release.

## Reporting a Vulnerability

Do not open a public issue, discussion, or pull request for a suspected
vulnerability.

Report it privately through either:

- a GitHub Security Advisory in this repository, preferred; or
- email to `bugbounty@debank.com`.

Include the affected commit or image tag, impact, reproduction steps, and a
proof of concept when available.

We aim to acknowledge reports within 72 hours and provide an initial assessment
within three to five business days. Fix timing depends on severity and whether
upstream coordination is required.

## Disclosure

Please allow time for a fix and release before public disclosure. Reporter
credit is provided unless anonymity is requested.
