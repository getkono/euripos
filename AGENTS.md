# euripos

An async Rust HTTP client with a platform-agnostic API and first-class integration on Linux, macOS, Windows, iOS and Android. `docs/scope.md` is normative: read it before any change.

## Rules

- Public API: no `cfg` on public items; platform facts surface as typed values that may be `Unknown`. Idiomatic Rust (Rust API Guidelines), checked statically (clippy pedantic, `cargo public-api`, `cargo-semver-checks`).
- Platform code lives in one internal module per platform behind `cfg(target_os)`; everything else branches on the capabilities it reports, never on `cfg`.
- One implementation per platform. A second implementation of the same concern on one platform needs an ADR in `docs/adr/` showing that no single option dominates.
- Optimizations are evidence-gated: a behaviour that departs from a plain hyper/hyper-util client names its metric and shows a netlab win over that baseline and over reqwest. Issues labelled `evidence-gated` are not done without that result.
- Runtime: tokio only, never abstracted over. No wasm target.
- Design precedes code: a design section in `docs/` (Problem / Constraints / Alternatives / Choice / Rationale / Validated by) lands before or with the code.

## Commits and pull requests

- Conventional Commits, enforced by convco. Types: feat fix docs test refactor perf build ci chore revert.
- Atomic commits; every commit builds and passes tests. Breaking changes use `!` and a `BREAKING CHANGE:` footer.
- Bug fixes: a red test commit first, then the fix.
- Pull requests are merged, not squashed or rebased.
- No `TODO` without an issue: write `TODO(#NN)`.
