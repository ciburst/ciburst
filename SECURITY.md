# Security

## Reporting a vulnerability

Open a confidential issue in [gitlab.com/ciburst/ciburst](https://gitlab.com/ciburst/ciburst/-/issues/new) (check "This issue is confidential"), not a public issue or merge request. Include the affected version, the impact and the steps that reproduce it. The maintainers answer in the issue and publish the fix and an advisory once a release carries it.

## Supported versions

The newest release of the newest major version, and `main`.

## Scope

The `ciburst` and `ciburst-vm-agent` binaries, the dispatcher function, the CI/CD component and the `cli` image. Weaknesses of Scaleway's or GitLab's own services go to them.

## Design

The security model, secrets and trust boundaries: `docs/SPEC.md`, section 10.
