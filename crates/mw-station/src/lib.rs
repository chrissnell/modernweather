//! Stations: `Transport`, `Session`, `StationIdentity`, identity resolution, and the
//! station components.
//!
//! The four-part decomposition of W2a §4a: a transport moves bytes, a session owns the
//! conversation, a codec (in `mw-codec`, which cannot see a socket) turns a frame into
//! readings, and identity resolution says which station they belong to. Forbidden to
//! depend on `mw-store`, `mw-api-legacy` or `mw-web`.
//!
//! ```text
//!  Transport ──bytes──▶ Session ──frames──▶ Codec ──(claim, PartialReading)──▶ admit
//!  (here)               (here, impure)      (mw-codec, pure)                   (mw-pipeline)
//!                                                                                   │
//!                                                      StationRegistry::resolve ◀────┘
//!                                                      (the one gate; §4a.6)
//! ```
//!
//! # Where the authentication is, and where it is not
//!
//! Not here. Nothing in this crate produces a [`mw_types::StationKey`]: a driver decides
//! what a frame *claimed* ([`StationIdentity::frame_meta`]) and
//! `mw_component::StationRegistry::resolve`, reached from inside `Ingress::admit`, decides
//! whether that claim is a station. So there is no envelope on this path before a station
//! exists, which is W2a §4a.6's structural claim rather than a rule a driver has to
//! remember. The Go gRPC receiver does the opposite — `station.go:174` stores the reading
//! and `:194` consults the registry afterwards — and that ordering is not expressible here.
//!
//! # What is here
//!
//! The declarations — [`Transport`], [`StationIdentity`], [`Session`], [`Endpoint`] — and
//! **one driver**: WeatherLink Live, in [`wll`], as the three components W2a **D-44**
//! decomposes it into. The other six drivers are M6-1's and M6-3's, and a configured
//! station with no driver is a plan error that names the work item ([`plan`]) rather than a
//! station that quietly reports nothing.
//!
//! Two things in this crate exist for a constraint from outside W2a. [`mdns`] is an
//! in-process multicast resolver, because **D-43**'s static musl linkage means
//! `getaddrinfo` cannot answer a `.local` name in the shipped binary (W8 **OQ-4**); and
//! [`http`] opens its own connection rather than taking one from a pool, because a station
//! read is one request every thirty seconds against an appliance.
//!
//! # The one socket this crate opens
//!
//! `ci/layers.toml`'s `only-mw-http-binds-a-socket` gives every *listener* to `mw-http` and
//! deliberately omits `UdpSocket::bind`, because §1.4's monopoly is over the socket a
//! request arrives on and a receive socket is not one. That asymmetry is what lets
//! [`wll::BroadcastListener`] exist, and it is the only place this crate uses it.

pub mod airgradient;
pub mod ambient;
pub mod campbell;
pub mod config;
pub mod davis;
pub mod driver;
pub mod endpoint;
pub mod http;
pub mod identity;
pub mod lines;
pub mod mdns;
pub mod metrics;
pub mod plan;
pub mod resolve;
pub mod session;
pub mod snowgauge;
pub mod stream;
pub mod testing;
pub mod transport;
pub mod wll;

pub use config::{StationConfig, StationRow};
pub use davis::DavisSession;
pub use driver::{Conversation, StreamStation};
pub use endpoint::{Endpoint, EndpointError};
pub use identity::{IdentityError, StationIdentity};
pub use lines::Lines;
pub use plan::{ambient_mount, fixed_receivers, register, FixedReceiver, StationPlanError};
pub use session::{ByteStream, NoSession, Session, SessionError, MAX_FRAME_BYTES};
pub use stream::{OpenError, SerialPort, TcpByteStream};
pub use transport::{HttpMethod, TlsFiles, Transport, TransportColumns, TransportError};
