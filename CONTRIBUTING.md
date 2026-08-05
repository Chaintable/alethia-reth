# Contributing

Thank you for contributing to Chaintable Alethia-Reth.

This repository is a maintained downstream of
[taikoxyz/alethia-reth](https://github.com/taikoxyz/alethia-reth). Changes that
reproduce on an unmodified upstream client should normally be proposed
upstream. Changes specific to `trace_debankBlock`, Chaintable output
compatibility, images, CI, and downstream integration belong here.

## Development Workflow

1. Create a branch from `main`.
2. Keep the change focused and avoid unrelated upstream refactors.
3. Add or update deterministic tests when behavior changes.
4. Run the required checks.
5. Open a draft pull request.

The workspace uses the Rust version pinned by `rust-toolchain.toml`. Install
`just` and `cargo-nextest`, then run:

```bash
just fmt-check
just clippy
just test
```

Do not remove or skip failing tests to make a change pass.

## Fixtures

Replay fixtures must come from public canonical chain data. Record the network,
block number, block hash, parent root, source RPC, collection time, and file
checksums. Never commit private RPC credentials or internal endpoints.

Keep fixture updates separate from unrelated source changes where practical.
Explain why each retained block is necessary.

## Pull Requests

Pull requests must include:

- the problem and behavior change;
- Taiko fork and compatibility impact;
- state-root, receipt, or block-file validation performed;
- exact local commands and results;
- known gaps and external validation still required.

Use Conventional Commit titles in imperative form with a lowercase subject,
for example:

```text
feat(rpc): add validated trace_debankBlock for Taiko
```

## Images and Releases

CI publishes commit-addressed images to
`public.ecr.aws/b2h7a5c4/chaintable/taiko-writer`. Release tags follow
`v<upstream-version>-ct.N`, for example `v1.3.0-ct.1`. Do not use mutable image
tags in deployments.

## Security

Do not disclose vulnerabilities publicly. Follow [SECURITY.md](SECURITY.md).

## License

Contributions are licensed under the repository's MIT License.
