# Certificate distrust in the TLS client

This proof of concept implements option 2: check the eight compromised public keys in
`saluki-tls`, while retaining registry WebPKI 0.103.13, Rustls 0.23.43, AWS-LC 1.17.3,
and AWS-LC-FIPS 0.13.16. It adds no dependencies or changes to `Cargo.lock`.
The alternative, option 1, ports the upstream check into vendored WebPKI.

## Verification

`ClientTLSConfigBuilder` installs a wrapper around `WebPkiServerVerifier`.
The wrapper uses Rustls's public `ParsedCertificate` API to extract the complete DER SPKI
of every presented certificate. It rejects any matching blocked key before invoking the
normal verifier. Hostname, validity, chain signatures, and handshake signatures remain
checked by Rustls. Comparing public keys also covers reissued and cross-signed certificates.

The wrapper filters matching keys from its own trust-anchor view because servers normally
omit roots. It retains unrelated roots, including the `emSign` and AAA roots used by the real
regression chains. It clones the root store only if a blocked anchor is present; the shared
default store remains unchanged. If filtering removes all anchors, construction fails.
An originally empty store retains its existing behavior: configuration succeeds, but no
server can authenticate.

Presented blocked keys produce `CertificateError::ApplicationVerificationFailure`.
A path depending solely on a removed, omitted anchor fails with `UnknownIssuer`.
Malformed presented certificates fail parsing; there is no fallback that accepts them.

The explicit local `danger_accept_invalid_certs` verifier and the exact-certificate IPC
verifier retain their existing behavior. Arbitrary complete configurations supplied through
`with_client_tls_config` remain caller-controlled. Incoming client-certificate verification
also remains outside this proof of concept; the WebPKI patch covers WebPKI client verifiers too.

## Proxy and platform coverage

The proxy library uses the supplied Saluki connector for the connection **to the proxy**.
For an HTTPS proxy, this runs the protected verifier.
The proxy library then performs a separate TLS handshake **through CONNECT to the endpoint**
using its own connector. Its default connector would bypass the wrapper.

This proof of concept explicitly supplies a protected TLS configuration for that second
handshake via `ProxyConnector::set_tls`. This preserves the previous separate native-root
configuration and avoids propagating an intentionally insecure local configuration into
the CONNECT handshake. The local HTTP regression exercises direct HTTPS, HTTPS through
an HTTP proxy, HTTPS through an HTTPS proxy, and distrust of the HTTPS proxy itself,
with successful controls for each route.

The Saluki builder uses the same WebPKI path verifier on all platforms. Windows changes the cryptography
provider to CNG, rather than selecting a separate certificate-verification API. The wrapper
has no platform exclusion or new AWS-LC feature activation. Runtime validation on macOS
does not establish Windows or Linux runtime compatibility.

The workspace also contains `rustls-platform-verifier` through `reqwest` and `kube` in
Panoramic and Antithesis tooling, outside the production binary's TLS graph. This wrapper
does not change those callers. A WebPKI patch likewise does not change native Windows
certificate-policy decisions made by a separate platform verifier.

## Differences from the WebPKI patch

| Case | Option 1: WebPKI path check | Option 2: wrapper |
| --- | --- | --- |
| Blocked leaf or intermediate required by the path | Rejects | Rejects |
| Same key in a reissued or cross-signed certificate | Rejects | Rejects |
| Blocked trust anchor omitted by the server | Rejects during path building | Removes anchor before path building |
| Blocked certificate supplied but unused | Can accept the valid path | Rejects the connection |
| Alternative valid path around a blocked issuer | Can select the valid path | Rejects if the blocked issuer was presented |
| Unused malformed or unsupported certificate | Can ignore it | Rejects while parsing all presented certificates |
| Other users of the patched WebPKI and incoming WebPKI client authentication | Covered by the dependency patch | Require separate integration |
| Certificate parser and cryptography | Uses WebPKI internals | Uses public Rustls APIs; unchanged cryptography |
| Maintenance | Vendored upstream source and patch lifecycle | Local verifier, key list, proxy wiring, and regression tests |

The wrapper parses each presented certificate an additional time and allocates its encoded
SPKI. The eight blocked SPKI values are 120 bytes each. No benchmark establishes the cost; this
work occurs during full certificate-verifying handshakes, not on each HTTP request.
The conservative treatment of unused certificates is a compatibility trade-off. This also
affects unused certificates with formats or critical extensions that the parser does not support.

The fixed keys originate in
[upstream commit f3caaa9f2ba94a42dcd31767e367688a416a3d87](https://github.com/rustls/webpki/commit/f3caaa9f2ba94a42dcd31767e367688a416a3d87).
Their encoding is documented beside the fixtures. Remove this workaround when an upstream
WebPKI version with the security fix is compatible with the approved crypto dependencies.
The draft includes a security release note; release-platform validation is still required.

## Local validation

On October 20, 2026, on macOS ARM64:

| Check | Result |
| --- | --- |
| `cargo check --locked --workspace` | Passed |
| `cargo check --locked --workspace --tests` | Passed |
| `cargo nextest run --locked -p saluki-tls -p saluki-io` | 258 passed |
| `cargo nextest run --locked -p saluki-tls -p saluki-io --features fips` | 257 passed |
| `make fmt` | Passed |
| `make check-all` | Passed, including the feature matrix and AIX type-check |

Both test modes verify twelve real affected chains against the unchanged WebPKI baseline
and the wrapper, with the same trusted roots and fixed validation time. The unaffected
control remains accepted. Local network tests exercise seven direct/proxy cases, and
in-memory handshakes exercise both TLS 1.2 and TLS 1.3.
The tests also cover omitted blocked trust anchors, explicit local bypass behavior,
unused blocked intermediates, malformed input, hostname mismatch, expiry, and bad signatures.
All eight key files were compared byte-for-byte with the upstream contents plus their DER header.

The checkout includes the exact already-merged policy update from
commit `1bb95f24d42f803f31870b2248f6501dcd40a06e` so its advisory checks use the approved
exceptions from main. This is separate from the certificate-distrust implementation.
`make test-all`, live external endpoint tests with a rebuilt binary, benchmarks, and Windows/Linux
runtime tests were not run for this proof of concept.
