//! **GRA-787's proof**: one serial line, two readers, the same bytes.
//!
//! M5-2's mechanism (b) diffs the two implementations by value on real data, which needs
//! both daemons fed the same readings. The WeatherLink Live gives that away — it broadcasts
//! over UDP and both can listen. A Campbell datalogger is on a serial line, and a serial
//! line has exactly one reader, so for such a station mechanism (b) does not exist without
//! a multiplexer in front of the port.
//!
//! `ser2net`'s `max-connections` is the documented way to build one. **The documentation is
//! not the proof.** The question these tests exist to answer is the one that decides whether
//! the comparison is worth anything:
//!
//! > does the multiplexer broadcast every byte to all connected clients, or does it hand
//! > bytes to whichever client reads first?
//!
//! If it is the second, the two daemons split the stream between them and *both* produce
//! gap-ridden data that still parses — which is the worst available outcome, because
//! nothing downstream can tell it from weather. So the assertion is byte-for-byte equality
//! of two concurrently connected clients' streams, and there are **two negative controls**
//! for it, because an identity assertion that cannot be made to fail proves nothing (W1
//! **D-10**):
//!
//! - [`the_control_bites_when_the_bridge_has_only_one_client_slot`] — the same apparatus
//!   with `max-connections: 1`, where the second client is refused and the assertion must
//!   go red;
//! - [`two_readers_of_one_character_device_do_not_both_get_the_stream`] — the same
//!   apparatus with the bridge taken out, which is the *real* failure mode and the reason
//!   `packaging/serial-bridge/README.md`'s cutover order is what it is. It opens the device
//!   with a plain `open(2)` rather than through `mw_station::stream::open`, which since
//!   GRA-788 takes `TIOCEXCL` and would refuse the second open: what is under test is the
//!   device, not this daemon's guard over it.
//!
//! # What runs these
//!
//! `ser2net` and `socat`, as real processes. Neither is a Rust dependency and neither is a
//! double: the binary under test is the one `packaging/serial-bridge/` deploys. A missing
//! binary is a **failure** rather than a skip, for the reason `emulator_campbell.rs` gives
//! for
//! `socat` — a gate that reports green while running nothing is
//! `docs/reconciliation.md` G-1. `#[ignore]`d so that `cargo test` on a machine with
//! neither stays green; `ci/serial-bridge/prove.sh` is what runs them and it asserts the
//! count.
//!
//! The station side of every test is a `socat` pty pair, so all of this is provable with no
//! hardware and without touching the live station.

use std::sync::Arc;
use std::time::Duration;

use mw_codec::CampbellAscii;
use mw_component::{ComponentId, Cx, Generation, Ingress};
use mw_station::driver::{Conversation, StreamStation};
use mw_station::testing::{RecordingIngress, Ser2netBridge, SENTINEL};
use mw_types::StationKey;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// How long any one read may wait with nothing arriving before the stream is called done.
const QUIET: Duration = Duration::from_millis(750);

/// A ceiling on a whole collection, so a wedged bridge fails rather than hangs.
const DEADLINE: Duration = Duration::from_secs(30);

/// What the datalogger says. `mw_codec::CampbellAscii`'s own keys, so that the bytes in
/// these tests are the bytes the driver will meet.
fn records(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 1..=n {
        out.extend_from_slice(
            &format!(
                "{{\"batt_volt\":13.2,\"airtemp_f\":{temp:.1},\"rh\":41.5,\"baro\":30.12,\
                 \"baro_temp_f\":70.1,\"wind_s\":7.5,\"seq\":{i}}}\n",
                temp = 40.0 + (i % 20) as f32,
            )
            .into_bytes(),
        );
    }
    out
}

async fn bridge_or_fail(name: &str, slots: u32) -> Ser2netBridge {
    Ser2netBridge::spawn(name, slots).await.expect(
        "`ser2net` and `socat` are test dependencies of GRA-787: the bridge under test is the \
         binary a deployment actually runs, so a missing one is a failure and not a skip. \
         `SER2NET` overrides the path (the Debian package installs it in /usr/sbin, which is \
         not on a non-root PATH) and `GENSIO_LIBRARY_PATH` is gensio's own.",
    )
}

/// Every test connects its clients, waits here, and only then lets the station speak.
///
/// `Ser2netBridge::wait_until_delivering` has why: the bridge opens the device on the first
/// client connection and drops anything that arrives while it is closed, so a test that
/// feeds the station immediately after connecting reads nothing and blames the bridge.
/// Only the clients that are *supposed* to be wired are passed — a client beyond
/// `max-connections` is holding a refusal that its test is about to assert on.
async fn delivering(bridge: &Ser2netBridge, clients: &mut [&mut tokio::net::TcpStream]) {
    assert!(
        bridge.wait_until_delivering(clients).await,
        "the bridge never delivered a byte to every client that should have been wired to it"
    );
}

/// Read until `want` bytes have arrived, or until the stream has been quiet for [`QUIET`].
///
/// Quiet rather than exact, because a client that is *supposed* to receive less than `want`
/// has to be able to say so instead of timing out — both negative controls need that.
async fn read_until_quiet(stream: &mut tokio::net::TcpStream, want: usize) -> Vec<u8> {
    // `want` is a ceiling on the read and a caller may pass `usize::MAX` to mean "until it
    // goes quiet", so it is not a capacity.
    let mut got = Vec::new();
    let overall = tokio::time::Instant::now() + DEADLINE;
    let mut buf = [0_u8; 16 * 1024];
    while got.len() < want && tokio::time::Instant::now() < overall {
        match tokio::time::timeout(QUIET, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => got.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    got
}

async fn connect(port: u16) -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("the bridge is listening")
}

/// **The deliverable.** Two clients connected at once, a known sequence pushed through the
/// device side, and both clients receive the identical byte stream — asserted byte for
/// byte, not eyeballed.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn both_clients_receive_the_identical_byte_stream() {
    let bridge = bridge_or_fail("campbell", 4).await;
    // Both clients first: a byte that arrives with nobody connected is dropped, because the
    // bridge is a fan-out and not a spool. That is a property of the thing, recorded in
    // `packaging/serial-bridge/README.md`, and it is why the station is fed after this line
    // rather than before it.
    let mut a = connect(bridge.port()).await;
    let mut b = connect(bridge.port()).await;
    delivering(&bridge, &mut [&mut a, &mut b]).await;

    let sent = records(200);
    bridge.feed(&sent).expect("the station writes to the line");

    let (got_a, got_b) = tokio::join!(
        read_until_quiet(&mut a, sent.len()),
        read_until_quiet(&mut b, sent.len())
    );

    assert_eq!(
        got_a,
        got_b,
        "the two clients did not receive the same bytes: {} vs {} bytes. If this is red, \
         mechanism (b) on the Campbell station is not available by this route, because the \
         two daemons are being handed different halves of one stream.",
        got_a.len(),
        got_b.len()
    );
    // And the identity is of the *whole* stream rather than of a shared prefix: two clients
    // that each received the first eleven bytes and nothing else would also be equal.
    assert_eq!(got_a, sent, "the stream was not delivered whole");
    assert_eq!(
        got_a.iter().filter(|b| **b == b'\n').count(),
        200,
        "two hundred records went in"
    );
}

/// **Negative control, for the assertion.** The same apparatus with one client slot: the
/// second client is answered and refused rather than joined to the stream, so the identity
/// assertion above must go red. If this test passes while
/// [`both_clients_receive_the_identical_byte_stream`] also passes, the assertion is
/// sensitive to the configuration it is supposed to be proving.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn the_control_bites_when_the_bridge_has_only_one_client_slot() {
    let bridge = bridge_or_fail("campbell-one-slot", 1).await;
    let mut a = connect(bridge.port()).await;
    let mut b = connect(bridge.port()).await;
    // Only `a`: `b` is the client this test expects to be refused, and the refusal is what
    // it asserts on.
    delivering(&bridge, &mut [&mut a]).await;

    let sent = records(50);
    bridge.feed(&sent).expect("the station writes to the line");

    let (got_a, got_b) = tokio::join!(
        read_until_quiet(&mut a, sent.len()),
        read_until_quiet(&mut b, sent.len())
    );

    assert_eq!(
        got_a, sent,
        "the client that holds the one slot still gets everything"
    );
    assert_ne!(
        got_a, got_b,
        "the control did not bite: a misconfigured bridge was indistinguishable from a working one"
    );
    // What the refused client gets instead, which is worth knowing by name: it is a line of
    // text on the same socket, so a Campbell reader counts it as one undecodable frame and
    // then sees EOF. That is the symptom to look for if
    // `mw_station_frames_total{outcome="undecodable"}` moves right after a cutover.
    assert_eq!(
        String::from_utf8_lossy(&got_b),
        "Port already in use\r\n",
        "ser2net's refusal is the message this programme expects to see in the logs"
    );
}

/// **Negative control, for the apparatus.** Take the bridge out and let two readers share
/// the character device, which is what happens if the multiplexer is started while an
/// older daemon is still holding the port.
///
/// `github.com/tarm/goserial` — `remoteweather`'s serial library — sets neither `TIOCEXCL`
/// nor `O_EXCL` nor a `flock`, so that second `open(2)` **succeeds**: the kernel hands each
/// byte to whichever reader asks first and there is no error anywhere to notice. This is
/// the failure the cutover order in `packaging/serial-bridge/README.md` exists to prevent,
/// and asserting it here is what makes that order a measured requirement rather than
/// caution.
#[tokio::test]
#[ignore = "needs `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn two_readers_of_one_character_device_do_not_both_get_the_stream() {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = std::env::temp_dir().join(format!("mw-two-readers-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let line = dir.join("line");
    let station = dir.join("station");
    let mut socat = tokio::process::Command::new("socat")
        .arg(format!("PTY,raw,echo=0,link={}", line.display()))
        .arg(format!("PTY,raw,echo=0,link={}", station.display()))
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("`socat` is a test dependency of this item");
    for _ in 0..200 {
        if line.exists() && station.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A plain `open(2)` with the flags any careful serial reader uses: `O_NONBLOCK` because
    // `open(2)` on a tty blocks until carrier, `O_NOCTTY` because this must not acquire a
    // controlling terminal. **Deliberately NOT `crate::stream::open`**, and that distinction
    // is the point of this test since GRA-788: `stream::open` now takes `TIOCEXCL` and an
    // exclusive `flock`, so it would refuse the second open and this test would measure its
    // own caller rather than the hazard. What is being measured is the *device's* behaviour
    // -- an unbridged character device admits a second reader and splits the stream -- which
    // is why the cutover order in `packaging/serial-bridge/README.md` exists and is also what
    // the Go daemon did. `stream::open`'s exclusivity is a second, independent guard over the
    // same hazard, not a replacement for the bridge.
    const O_NONBLOCK_NOCTTY: i32 = 0o4000 | 0o400;
    let open_line = || {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK_NOCTTY)
            .open(&line)
    };
    let mut first = open_line().expect("the incumbent opens the line");
    let mut second = open_line().expect(
        "a second plain `open(2)` of the same character device SUCCEEDS -- that is the whole \
         hazard, and if this ever starts failing the cutover order could be relaxed",
    );

    let sent = records(200);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&station)
        .and_then(|mut d| std::io::Write::write_all(&mut d, &sent))
        .expect("the station writes to the line");

    // Drain both for a while. Non-blocking, so `WouldBlock` is "nothing right now".
    let mut got = [Vec::new(), Vec::new()];
    let until = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < until {
        for (slot, reader) in [&mut first, &mut second].into_iter().enumerate() {
            let mut buf = [0_u8; 16 * 1024];
            match reader.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => got[slot].extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = socat.start_kill();
    std::fs::remove_dir_all(&dir).ok();

    assert_ne!(
        got[0], got[1],
        "two readers of one character device received identical streams, which would mean a \
         serial port fans out on its own and GRA-787's multiplexer is unnecessary"
    );
    assert!(
        got[0] != sent || got[1] != sent,
        "both readers received the whole stream, which a single-reader device cannot do"
    );
}

/// A client beyond the configured limit is refused, and — the part that matters
/// operationally — the readers already connected do not notice. A bridge that displaced an
/// incumbent instead (`ser2net`'s `kickolduser`) would make a careless third connection
/// take production off the air.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn a_client_over_the_limit_is_refused_and_the_readers_already_connected_are_undisturbed() {
    let bridge = bridge_or_fail("campbell-two-slots", 2).await;
    let mut a = connect(bridge.port()).await;
    let mut b = connect(bridge.port()).await;
    let mut c = connect(bridge.port()).await;
    delivering(&bridge, &mut [&mut a, &mut b]).await;

    let sent = records(50);
    bridge.feed(&sent).expect("the station writes to the line");

    let (got_a, got_b, got_c) = tokio::join!(
        read_until_quiet(&mut a, sent.len()),
        read_until_quiet(&mut b, sent.len()),
        read_until_quiet(&mut c, sent.len())
    );
    assert_eq!(got_a, sent, "the first client kept its stream");
    assert_eq!(got_b, sent, "the second client kept its stream");
    assert_eq!(String::from_utf8_lossy(&got_c), "Port already in use\r\n");
}

/// One client stopping dead does not cost the other a byte.
///
/// This is the property that says the two daemons are independent once the bridge is in
/// front of the line: if `remoteweather` wedges, `modernweather` keeps reading, and the
/// reverse. The volume is chosen to be absurd against the station — 6,000 Campbell records
/// is over three hours at its two-second output period — while one client reads nothing at
/// all.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn a_client_that_stops_reading_does_not_starve_the_other() {
    let bridge = bridge_or_fail("campbell-stalled-peer", 4).await;
    let mut reading = connect(bridge.port()).await;
    let mut stalled = connect(bridge.port()).await;
    // Both are wired first, and only then does one of them stop reading -- which is the
    // event under test. A client that was never wired would prove nothing.
    delivering(&bridge, &mut [&mut reading, &mut stalled]).await;

    let sent = records(6_000);
    bridge.feed(&sent).expect("the station writes to the line");
    let got = read_until_quiet(&mut reading, sent.len()).await;

    assert_eq!(
        got.len(),
        sent.len(),
        "the reading client lost {} bytes while its peer read nothing",
        sent.len().saturating_sub(got.len())
    );
    assert_eq!(got, sent);
    drop(stalled);
}

/// **Done-when 4, with no hardware**: `modernweather`'s own Campbell driver reads the
/// bridge while a second client reads the same bytes at the same time.
///
/// The second client stands in for `remoteweather`, which is the right stand-in for this
/// assertion: what is under test is that the bridge serves a real driver and a second
/// reader concurrently, and `remoteweather`'s Campbell driver over `TcpOut` is the same
/// `read` loop on the same socket. Every record the driver admitted is a record the other
/// client saw, and the raw stream is the whole of what was sent.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn the_campbell_driver_reads_the_bridge_while_a_second_client_reads_the_same_bytes() {
    let bridge = bridge_or_fail("campbell-with-driver", 4).await;
    let mut peer = connect(bridge.port()).await;

    let ingress = RecordingIngress::accepting();
    let station = Box::new(
        StreamStation::connect(
            &bridge.transport(),
            Conversation::Lines,
            Arc::new(CampbellAscii::default()),
            StationKey::new("suncrest:meadow"),
            Arc::clone(&ingress) as Arc<dyn Ingress>,
        )
        .await
        .expect("the driver connects to the bridge"),
    );
    let token = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Generation::ZERO);
    let context = Cx::new(
        ComponentId::new("station:campbell:suncrest:meadow").expect("a legal component id"),
        token.clone(),
        Generation::ZERO,
        rx,
    );
    // `clippy.toml` forbids `spawn` in the daemon and this test is the supervisor, which is
    // the note `emulator_campbell.rs` carries for the same line.
    #[allow(clippy::disallowed_methods)]
    let driving = tokio::task::spawn(async move { station.run(context).await });

    // The driver is a client of the bridge too, and its readiness is observable in what it
    // has admitted. So the sentinels run until the peer has bytes *and* the driver has a
    // reading, and the admissions the sentinels produced are then counted out.
    delivering(&bridge, &mut [&mut peer]).await;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while ingress.admissions().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the driver admitted nothing through the bridge"
        );
        bridge
            .feed(SENTINEL)
            .expect("the station writes to the line");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = read_until_quiet(&mut peer, usize::MAX).await;
    let settled = ingress.admissions().len();

    let sent = records(100);
    bridge.feed(&sent).expect("the station writes to the line");

    let got_peer = read_until_quiet(&mut peer, sent.len()).await;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while ingress.admissions().len() < settled + 100 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the driver admitted only {} of 100 records through the bridge",
            ingress.admissions().len() - settled
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), driving).await;

    assert_eq!(
        got_peer, sent,
        "the second client received the whole stream"
    );
    let admissions = ingress.admissions();
    assert_eq!(
        admissions.len(),
        settled + 100,
        "one reading per record, and no record admitted twice"
    );
    for admission in &admissions[settled..] {
        assert_eq!(admission.batch.len(), 1);
        let r = &admission.batch[0].values;
        assert_eq!(r.barometer, Some(30.12));
        assert_eq!(r.wind_speed, Some(7.5));
        assert!(
            r.out_temp.is_some_and(|t| (40.0..=60.0).contains(&t)),
            "the driver decoded a temperature the generator never wrote: {r:?}"
        );
    }
}

/// Once the bridge has the line, nothing else can take it: `ser2net` sets `TIOCEXCL`, so a
/// stray serial reader gets `EBUSY` rather than half the stream.
///
/// This is the fail-safe half of the asymmetry
/// [`two_readers_of_one_character_device_do_not_both_get_the_stream`] measures. Started in
/// the right order, a daemon that is accidentally left pointing at the serial port logs
/// `failed to open serial port … device or resource busy` and retries — loud, and not a
/// split stream. Since GRA-788 that daemon would also have refused its own second opener,
/// but this assertion is about the bridge holding the line and does not depend on it.
#[tokio::test]
#[ignore = "needs `ser2net` and `socat`; `ci/serial-bridge/prove.sh` runs it"]
async fn the_bridge_owns_the_line_exclusively_once_it_has_a_client() {
    use std::os::unix::fs::OpenOptionsExt;

    let bridge = bridge_or_fail("campbell-exclusive", 4).await;
    // The device is opened on the first client connection, not at startup, which is also
    // why `ser2net` starts happily with the cable unplugged
    // (`packaging/serial-bridge/README.md`). `Ser2netBridge::spawn` connects a throwaway
    // client of its own, so by the time a test runs the device is already held -- the
    // *other* half of this property, that an unbridged character device admits a second
    // plain `open(2)`, is what
    // `two_readers_of_one_character_device_do_not_both_get_the_stream` opens twice.
    //
    // `ser2net`'s `TIOCEXCL` and the one `mw_station::stream::open` has taken since GRA-788
    // are the same mechanism pointed at the same hazard from the two ends. This test asserts
    // the bridge's, which is the one that matters here: the bridge is what owns the line in
    // N-20's deployment, and this daemon reaches it over `tcp_out`.
    const O_NONBLOCK_NOCTTY: i32 = 0o4000 | 0o400;
    let open_line = || {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK_NOCTTY)
            .open(bridge.line_side())
    };

    let mut client = connect(bridge.port()).await;
    delivering(&bridge, &mut [&mut client]).await;

    let e = open_line().expect_err("a stray reader must not be able to open the line");
    assert_eq!(
        e.raw_os_error(),
        Some(16),
        "EBUSY is what `TIOCEXCL` produces and what `remoteweather` would log: {e}"
    );
}
