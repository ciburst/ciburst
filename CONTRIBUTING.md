# Contributing

Issues and merge requests: [gitlab.com/ciburst/ciburst](https://gitlab.com/ciburst/ciburst). The GitHub mirror is read-only.

## Developer Certificate of Origin

Every commit carries a `Signed-off-by` line with the author's name and email (`git commit -s`). It certifies the [Developer Certificate of Origin 1.1](https://developercertificate.org/): the author wrote the change or has the right to submit it under the project's license. Contributions are licensed under Apache-2.0 (`LICENSE`); copyright stays with "The ciburst Authors" (`NOTICE`).

## A change

1. Read `docs/SPEC.md`: decisions in section 0, then every binding rule. Code cites it as `spec 6.3` or `D53`.
2. A design change starts in the spec: a decision row in section 0 and its rules in the section it governs, before code. A decision it replaces moves to the dropped table with the reason.
3. Every external API is tested against an in-process fake that is as strict as the real service. A live finding becomes a rule of the fake and a test that fails without the fix.
4. Before every commit, the checks of `AGENTS.md`, section Work on this repository, pass.
5. A merge request names the decision or spec section it implements and how it was checked.

The repository holds no secrets and no identifying data: no real hostnames, ids, emails or personal namespaces. Values of a deployment live in CI/CD variables or in `.local/` (gitignored).

## Conduct

`CODE_OF_CONDUCT.md` applies to every space of the project. Security issues: `SECURITY.md`.
