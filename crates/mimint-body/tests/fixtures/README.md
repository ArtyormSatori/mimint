# Test-only TLS fixtures (P4.3)

`android-test-cert.pem` / `android-test-key.pem` are a self-signed
test identity for the fake Android shell's pinned-TLS tests
(`tests/android_conformance.rs`). They were generated with:

```sh
openssl req -x509 -newkey rsa:2048 -keyout android-test-key.pem \
  -out android-test-cert.pem -days 3650 -nodes -subj '/CN=127.0.0.1' \
  -addext 'subjectAltName=IP:127.0.0.1'
```

SHA-256 fingerprint of the certificate DER (as printed by
`openssl x509 -fingerprint -sha256` at generation time):

```text
8F:08:2B:D5:36:61:E8:04:3B:B7:DA:67:BA:17:A0:9F:28:33:59:FB:EC:8A:47:86:0D:58:92:D4:0B:EE:D5:B5
```

The tests compute the fingerprint from the certificate at runtime
and pin that — the value above is a cross-check for humans, not
an input to any test.

**Never use this identity anywhere real.** The private key is
committed to the repository on purpose (tests must run without
secret provisioning) and must be treated as public. The real
Kotlin shell generates its own identity on first run and shows
its fingerprint at pairing time; nothing in production reads
these files.
