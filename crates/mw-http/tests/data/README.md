# Test certificates

A throwaway CA and two leaves it signed, for `tests/tls.rs`. Generated once with:

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca-key.pem
openssl req -x509 -new -key ca-key.pem -out ca-cert.pem -days 36500 \
  -subj "/CN=mw-http test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"

# then, for CN=exact.test and CN=*.wild.test:
openssl ecparam -name prime256v1 -genkey -noout -out LEAF-key.pem
openssl req -new -key LEAF-key.pem -out LEAF.csr -subj "/CN=NAME"
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n\
extendedKeyUsage=serverAuth\nsubjectAltName=DNS:NAME\n' > LEAF.ext
openssl x509 -req -in LEAF.csr -CA ca-cert.pem -CAkey ca-key.pem -CAcreateserial \
  -out LEAF-cert.pem -days 36500 -extfile LEAF.ext
```

**The CA's private key is deliberately not here**: nothing needs to sign anything again, and a
signing key in a repository is a signing key in a repository. The three that are here are public
material plus two leaf keys that protect nothing.

**A CA and leaves rather than two self-signed certificates**, which was the first attempt:
`openssl req -x509` sets `basicConstraints=CA:TRUE`, and `webpki` rejects such a certificate as an
end entity (`CaUsedAsEndEntity`) — correctly, and the error is worth knowing about because it looks
like a trust-store problem and is not.

**Checked in rather than generated at test time**, because generating one needs a certificate
library this workspace does not have and `DESIGN.md` D-32 governs the daemon's dependencies — a
dev-dependency on `rcgen` would be an amendment for files that change never. `-days 36500` so that
the test does not acquire an expiry date.

`tests/tls.rs` is the only reader, and it trusts `ca-cert.pem` and nothing else. `wild-*` exists so
that **DEV-6**'s single-label wildcard match is asserted against a real handshake and not only
against the store's unit tests.
