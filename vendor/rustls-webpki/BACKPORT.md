# Temporary certificate-distrust backport

This is WebPKI 0.103.13 with the security fix from
[upstream commit f3caaa9f2ba94a42dcd31767e367688a416a3d87](https://github.com/rustls/webpki/commit/f3caaa9f2ba94a42dcd31767e367688a416a3d87),
released upstream in 0.103.16 via [PR 546](https://github.com/rustls/webpki/pull/546).
The workspace `[patch.crates-io]` entry applies it to every consumer of this WebPKI version,
including Rustls connections to HTTPS proxies.

## Why the backport exists

WebPKI 0.103.16 requires `aws-lc-rs ^1.18`. Saluki retains `aws-lc-rs <1.18` to keep
its existing FIPS module. This copy retains the 0.103.13 production dependencies:
the main workspace lockfile still selects `aws-lc-rs 1.17.3` and `aws-lc-fips-sys 0.13.16`.
No cryptographic implementation or trust-store loading behavior changes.

The patch rejects any certificate path containing one of eight compromised public keys,
including when that key occurs in a leaf, intermediate, or trusted root. It compares
SPKI SEQUENCE contents, not certificate fingerprints, so reissuance and cross-signing
do not bypass rejection.

## Provenance and local differences

- Production sources originate from the published `rustls-webpki 0.103.13` crate.
- Unit-test support and fixtures originate from the upstream `v/0.103.13` source archive.
  The original ISC license and Chromium fixture license are retained.
- The license inventory tool excludes path dependencies. The generated CSV therefore no
  longer lists this crate; its origin, copyright, and full ISC terms are preserved in the
  top-level `NOTICE`, which is shipped in the container and host release packages.
- The security commit changes `src/end_entity.rs`, `src/error.rs`, and `src/verify_cert.rs`,
  and adds eight public SPKI files under `src/data/distrusted/`.
- The new blocklist constant and test import are placed at module scope to follow local style.
  The check and regression-test behavior match the upstream release-branch patch.
- The normalized crate manifest retains production dependencies. Development dependencies
  used only by omitted benchmarks and integration tests are removed. The standalone
  lockfile keeps the unit tests on the same AWS-LC versions as the main workspace.
- This crate is excluded from the workspace to preserve upstream formatting and dependency
  declarations. Its tests run separately through `make test-webpki-backport`, also invoked
  by `make test`. Workspace feature checks enumerate workspace members rather than scanning
  every manifest, so this external crate is not treated as a Saluki package.

## Validation

```sh
make test-webpki-backport
cargo nextest run --locked -p saluki-tls certificate_distrust_tests
cargo nextest run --locked -p saluki-tls --features fips certificate_distrust_tests
make check-fips-module
```

The upstream tests exercise blocked leaves, intermediates, roots, cross-signs, unused
intermediates, and valid alternative paths. Saluki's regression tests use captured real
chains from twelve affected endpoints and an unaffected control at a fixed validation
time, with both emSign and AAA roots still trusted. A missing root or expired certificate
does not satisfy the regression: it requires the explicit `CertDistrusted` error.

The local macOS ARM64 ADP before/after test on October 20, 2026 used the same trust
bundle for both builds. The unmodified verifier accepted all twelve affected endpoints;
the backport rejected all twelve. Both builds accepted the unaffected control. This
validates the verifier change independently of root removal; release-platform testing
is still required.

## Security advisories

Advisory scanners such as `cargo-deny` and `cargo audit` match RustSec advisories against
crates.io packages, so they are not expected to report advisories for this path dependency.
Until this backport is removed, check new
[RustSec advisories for `rustls-webpki`](https://rustsec.org/packages/rustls-webpki.html) and
[upstream releases](https://github.com/rustls/webpki/releases) by hand, and apply any fix that
affects 0.103.13 here. Renovate also skips this directory through the `vendor` ignore in its
`config:recommended` preset, so it does not propose updates for this crate either.

## Removal

Remove the path override, workspace exclusion, this directory, and the standalone test
target when an upstream version with this fix can resolve against the approved crypto
dependencies. Regenerate the license CSV and remove the vendored notice when registry
attribution is restored. Update the main lockfile and rerun the certificate and FIPS checks.
Keep the Saluki regression fixtures to prevent a later dependency change from silently
restoring trust. The version remains 0.103.13 because this is a documented backport,
not an unmodified upstream 0.103.16 release.
