# Test-only TLS files

These certificates and keys exist only for the tests in
`crates/anvil-core/src/net/tests.rs`. They protect nothing: the private
keys are public in this repository. Never trust `ca.pem` anywhere else.

- `ca.pem`: a self-signed certificate authority (its key was discarded).
- `server.pem`, `server.key`: a server certificate for `localhost` and
  `127.0.0.1`, signed by `ca.pem`.
- `client.pem`, `client.key`: a client certificate for mutual TLS, signed
  by `ca.pem`.

All are EC P-256 and valid for 100 years. They were made with OpenSSL:
`genpkey -algorithm EC`, `req -x509` for the CA, and `x509 -req` with
`extendedKeyUsage=serverAuth` (and `subjectAltName=DNS:localhost,IP:127.0.0.1`)
or `extendedKeyUsage=clientAuth`.
