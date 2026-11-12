# Test certificates for the ingest socket

A throwaway peer CA and four leaves it signed, for `src/x509.rs`, `src/handshake.rs` and
`tests/ingest.rs`. `crates/mw-http/tests/data/README.md` is the same arrangement for the same
reason, and that reason is worth repeating: **generating one at test time needs a certificate
library this workspace does not have**, and `DESIGN.md` D-32 governs the dependencies — a
dev-dependency on `rcgen` would be an amendment for files that change never.

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca-key.pem
openssl req -x509 -new -key ca-key.pem -out ca-cert.pem -days 36500 \
  -subj "/CN=mw-collector test peer CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"

# then, per leaf, with the SAN from the table below:
openssl ecparam -name prime256v1 -genkey -noout -out LEAF-key.pem
openssl req -new -key LEAF-key.pem -out LEAF.csr -subj "/CN=LEAF"
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n\
extendedKeyUsage=clientAuth\nsubjectAltName=SAN\n' > LEAF.ext
openssl x509 -req -in LEAF.csr -CA ca-cert.pem -CAkey ca-key.pem -CAcreateserial \
  -out LEAF-cert.pem -days 36500 -extfile LEAF.ext

# and the pin an operator would be asked to configure, which is also what
# `crate::x509::spki` + `crate::handshake::sha256` must reproduce byte for byte:
openssl x509 -in LEAF-cert.pem -pubkey -noout \
  | openssl pkey -pubin -outform der | openssl dgst -sha256 -r | cut -d' ' -f1
```

| Leaf | `subjectAltName` | What it is for |
|---|---|---|
| `peer-a` | `URI:urn:modernweather:node:0193f3c2-7a10-7b64-9c2e-4f1b8a6d0e11` | the `mtls-ca` path: a chain-validated certificate whose SAN URI names a node |
| `peer-b` | `URI:urn:modernweather:node:0193f3c2-8b20-7c71-ae3f-10cc92d4b7aa` | a **second** valid peer, so that "the CA signed it" and "this is the peer I think it is" are distinguishable |
| `pinned` | `URI:urn:modernweather:node:0193f3c2-9c30-7d8e-b4a1-77de3b02f5c3` | the `mtls-pinned` path, where the SPKI digest is the whole decision and the chain is irrelevant |
| `peer-dns-only` | `DNS:dns-only.test` | a certificate the CA signed that names **no node**, which must resolve to nobody rather than to the only peer there is |
| `collector-localhost` | `DNS:localhost,IP:127.0.0.1` | the **server** certificate the ingest socket presents in `crates/modernweather/tests/collector_two_node.rs`. `extendedKeyUsage=serverAuth`, which is the one leaf here that is not a client |

`LEAF-spki-sha256.txt` beside each one is `openssl`'s own digest of that leaf's public key.
`src/x509.rs`'s `the_spki_is_the_bytes_openssl_hashes` asserts against it, which is what makes
the hand-written DER walk checkable against a tool nobody here wrote: a pin an operator
computes with `openssl` and a pin this build computes have to be the same string, or the
`mtls-pinned` configuration in `docs/collector.md` §8.4 cannot be followed.

**The CA's private key is deliberately not here.** Nothing needs to sign anything again. What
is here is public material plus five leaf keys that protect nothing — the leaf keys are
present because `crates/modernweather/tests/collector_two_node.rs` completes a real mTLS
handshake over a loopback socket, and both ends need their keys.

`-days 36500` so that no test acquires an expiry date.

**A CA and leaves rather than self-signed certificates**, for the reason `mw-http`'s README
records: `openssl req -x509` sets `basicConstraints=CA:TRUE` and `webpki` rejects such a
certificate as an end entity (`CaUsedAsEndEntity`). That matters here even for the *pinned*
path, where there is no chain to validate: the handshake still presents the leaf as an end
entity.
