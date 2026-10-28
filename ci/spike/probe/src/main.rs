//! Cross-compilation probe for the parts of the chosen crate stack that are not pure Rust.
//!
//! The ARMv6 question in W8 is load-bearing, and the only honest way to answer it is to
//! build and *run* the C-containing dependencies on an ARMv6 CPU. This binary is the
//! smallest thing that exercises all three of them:
//!
//! * `sqlx` with the SQLite backend, which links `libsqlite3-sys` with bundled C — the
//!   dependency the W8 prompt was worried about;
//! * a rustls crypto provider (`ring` or `aws-lc-rs`), which is C and hand-written
//!   assembly, and is a bigger ARMv6 risk than SQLite is;
//! * `argon2`, `zerocopy`, `axum` and `tokio`, which are pure Rust but pull in the
//!   wide-integer and unaligned-access paths where a 32-bit strict-alignment target
//!   diverges from x86_64.
//!
//! It is kept out of the main workspace deliberately (its own `[workspace]` table): it is
//! platform test infrastructure, not part of the daemon, and W3 owns what lives in
//! `crates/`.
//!
//! Run it with `ci/spike/armv6-cross-spike.sh`, which builds it for a target and executes
//! the result under qemu with the CPU model pinned to the hardware in question.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Row;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("arch={} pointer_width={}", std::env::consts::ARCH, usize::BITS);

    // SQLite through sqlx: bundled C, compiled by cc-rs for this target.
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect("sqlite::memory:")
        .await?;
    sqlx::query("CREATE TABLE obs (t INTEGER PRIMARY KEY, outtemp REAL)")
        .execute(&pool)
        .await?;
    for i in 0..1000i64 {
        sqlx::query("INSERT INTO obs (t, outtemp) VALUES (?1, ?2)")
            .bind(i)
            .bind(20.0f64 + (i as f64) / 100.0)
            .execute(&pool)
            .await?;
    }
    let row = sqlx::query("SELECT count(*) AS n, sum(outtemp) AS s FROM obs")
        .fetch_one(&pool)
        .await?;
    let n: i64 = row.try_get("n")?;
    let s: f64 = row.try_get("s")?;
    // 24995.0 exactly: a float sum that must agree across architectures, which is the
    // same property the aggregate hierarchy depends on (DESIGN.md D-14).
    println!("sqlite rows={n} sum={s:.4}");
    let ver = sqlx::query("SELECT sqlite_version() AS v")
        .fetch_one(&pool)
        .await?;
    println!("sqlite_version={}", ver.try_get::<String, _>("v")?);

    // Argon2id, which is what the management password will be hashed with (DESIGN.md D-35).
    {
        use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
        use argon2::Argon2;
        let salt = SaltString::generate(&mut OsRng);
        let h = Argon2::default()
            .hash_password(b"correct horse", &salt)
            .expect("argon2");
        println!("argon2_ok len={}", h.to_string().len());
    }

    #[cfg(feature = "rustls")]
    {
        let p = provider();
        println!("rustls_suites={} kx_groups={}", p.cipher_suites.len(), p.kx_groups.len());

        let mut buf = [0u8; 32];
        p.secure_random.fill(&mut buf).expect("secure_random");
        println!("secure_random_nonzero={}", buf.iter().any(|b| *b != 0));

        let hp = p
            .cipher_suites
            .iter()
            .find_map(|s| s.tls13().map(|t| t.common.hash_provider))
            .expect("a TLS 1.3 suite");
        let mut ctx = hp.start();
        ctx.update(b"modernweather");
        println!("hash={:?} len={}", hp.algorithm(), ctx.finish().as_ref().len());

        // A real key exchange, because the assembly in a crypto provider is where a
        // 32-bit ARM target actually breaks. Two halves of the same group must agree.
        //
        // Hybrid post-quantum groups are asymmetric by construction — the client and
        // server shares are different sizes — so this symmetric check does not apply to
        // them and a failure there says nothing about the target. Those are reported
        // rather than asserted.
        for g in p.kx_groups.iter() {
            let hybrid = format!("{:?}", g.name()).contains("MLKEM");
            let a = g.start().expect("kx a");
            let b = g.start().expect("kx b");
            let a_pub = a.pub_key().to_vec();
            let b_pub = b.pub_key().to_vec();
            match (a.complete(&b_pub), b.complete(&a_pub)) {
                (Ok(sa), Ok(sb)) => println!(
                    "kx {:?} agreed={} secret_len={}",
                    g.name(),
                    sa.secret_bytes() == sb.secret_bytes(),
                    sa.secret_bytes().len()
                ),
                (Err(e), _) | (_, Err(e)) if hybrid => {
                    println!("kx {:?} skipped (hybrid, asymmetric by design): {e}", g.name())
                }
                (Err(e), _) | (_, Err(e)) => {
                    return Err(format!("kx {:?} failed on this target: {e}", g.name()).into())
                }
            }
        }
    }

    #[cfg(feature = "rustls")]
    tls_handshake()?;

    println!("OK");
    Ok(())
}

/// A complete TLS 1.3 handshake plus application data, both ends in this process over a
/// byte buffer. This is the part that matters on ARMv6: it executes the AEAD and the
/// signature paths, which is where a crypto provider's hand-written assembly lives.
/// Initialising a provider proves nothing — the assembly is only reached by use.
#[cfg(feature = "rustls")]
fn tls_handshake() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

    let provider = Arc::new(provider());
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert = CertificateDer::from(issued.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(issued.signing_key.serialize_der())?;

    let server_config = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)?;

    let mut roots = RootCertStore::empty();
    roots.add(cert)?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();

    // Held as the `Connection` enum so one pump function can drive either end.
    let mut client = rustls::Connection::Client(ClientConnection::new(
        Arc::new(client_config),
        ServerName::try_from("localhost")?,
    )?);
    let mut server =
        rustls::Connection::Server(ServerConnection::new(Arc::new(server_config))?);

    // Pump bytes between the two until neither has anything to send.
    for _ in 0..32 {
        if !pump(&mut client, &mut server)? && !pump(&mut server, &mut client)? {
            break;
        }
    }
    if client.is_handshaking() || server.is_handshaking() {
        return Err("TLS handshake did not complete".into());
    }

    // Application data in both directions, so the AEAD seals and opens for real.
    use std::io::{Read, Write};
    client.writer().write_all(b"ping")?;
    pump(&mut client, &mut server)?;
    let mut got = [0u8; 4];
    server.reader().read_exact(&mut got)?;
    server.writer().write_all(b"pong")?;
    pump(&mut server, &mut client)?;
    let mut back = [0u8; 4];
    client.reader().read_exact(&mut back)?;

    let suite = client
        .negotiated_cipher_suite()
        .ok_or("no negotiated cipher suite")?;
    println!(
        "tls_handshake suite={:?} roundtrip={}",
        suite.suite(),
        &got == b"ping" && &back == b"pong"
    );
    if &got != b"ping" || &back != b"pong" {
        return Err("TLS application data did not round-trip".into());
    }
    Ok(())
}

/// Move whatever `from` wants to send into `to`. Returns whether anything moved.
#[cfg(feature = "rustls")]
fn pump(
    from: &mut rustls::Connection,
    to: &mut rustls::Connection,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut wire = Vec::new();
    while from.wants_write() {
        from.write_tls(&mut wire)?;
    }
    if wire.is_empty() {
        return Ok(false);
    }
    let mut cursor = std::io::Cursor::new(&wire);
    while (cursor.position() as usize) < wire.len() {
        to.read_tls(&mut cursor)?;
        to.process_new_packets()?;
    }
    Ok(true)
}

#[cfg(feature = "tls-ring")]
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(all(feature = "tls-awslc", not(feature = "tls-ring")))]
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}
