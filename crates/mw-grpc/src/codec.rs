//! The two `tonic::codec::Codec` implementations this crate's services use, and why neither
//! of them is a generated one.
//!
//! A `tonic` service is normally generated from a `.proto` by `tonic-prost-build`, which
//! emits a client, a server trait, and a `ProstCodec` over message types it also emits.
//! Neither service here is generated, and the reason is different for each.
//!
//! # The snow gauge: one schema, and it is `mw-codec`'s
//!
//! `mw_codec::SnowGaugeProto` **is** the snow gauge's schema (that module has why: the
//! layer rule puts `mw-proto` in `mw-codec`'s own layer, so the codec declares the two
//! fields it needs and `prost` discards the rest). Generating a second `Reading` here would
//! be a second declaration of the same message, in a different crate, free to disagree with
//! the one the golden tests are written against — and the only way to get a frame from one
//! to the other would be to re-encode what had just been decoded.
//!
//! [`BytesCodec`] removes the question. The client asks `tonic` for the **message bytes**
//! and hands them straight to the pure codec, so there is one schema, one decode, and no
//! round trip.
//!
//! # The gRPC receiver: the message is already generated, and `Empty` is not a message
//!
//! `WeatherV1.SendWeatherReadings` takes a stream of readings and answers
//! `remoteweather.v1.Empty`. The reading is `modernweather.v1.Reading` on the wire — the
//! registry's own schema, whose field numbers were bootstrapped from the Go
//! `WeatherReading` and are identical, which is what makes a Go-era satellite's readings
//! decodable here at all. So the message type exists; what does not is `Empty`, and an empty
//! protobuf message is **zero bytes**.
//!
//! [`ReadingCodec`] is therefore `prost` in one direction and nothing in the other: it
//! decodes [`mw_proto::wire::Reading`] and encodes `()` by writing no bytes, which is
//! exactly what `Empty` is on the wire.
//!
//! # What is given up by not generating
//!
//! The method **path** is a constant rather than something the generator derives from the
//! service name, so a typo in it is a client that compiles and gets `Unimplemented`. Both
//! paths are declared beside the schema they belong to — `mw_codec::STREAM_READING` for the
//! gauge, [`crate::grpcrx::SEND_WEATHER_READINGS`] for the receiver — and each has a test
//! asserting the exact string, because that string is a wire contract.

use prost::bytes::{Buf, BufMut};
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::Status;

/// A codec that does not decode: the message's bytes, verbatim, in both directions.
///
/// `Encode` is a `Vec<u8>` so that a request message can be sent as the bytes it encodes
/// to — which for the snow gauge's `StreamRequest` is none at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct BytesCodec;

impl Codec for BytesCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = BytesCodec;
    type Decoder = BytesCodec;

    fn encoder(&mut self) -> Self::Encoder {
        *self
    }

    fn decoder(&mut self) -> Self::Decoder {
        *self
    }
}

impl Encoder for BytesCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for BytesCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        Ok(Some(take_all(src)))
    }
}

/// The whole of what [`BytesCodec`] decodes, over any buffer.
///
/// A free function because `tonic::codec::DecodeBuf` has no public constructor, so the only
/// way to test a `Decoder` impl is to run a gRPC call — which is the emulator level, not
/// this one. Factoring the decision out means level 1 can assert it over a byte slice,
/// which is what every other decoder in this workspace is tested as.
fn take_all(src: &mut impl Buf) -> Vec<u8> {
    // `tonic` has already removed the five-byte gRPC length prefix and handed over exactly
    // one message, so "the rest of the buffer" is the message.
    let len = src.remaining();
    src.copy_to_bytes(len).to_vec()
}

/// `prost` in, nothing out: decodes [`mw_proto::wire::Reading`] and encodes the empty
/// response message.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadingCodec;

impl Codec for ReadingCodec {
    type Encode = ();
    type Decode = mw_proto::wire::Reading;
    type Encoder = ReadingCodec;
    type Decoder = ReadingCodec;

    fn encoder(&mut self) -> Self::Encoder {
        *self
    }

    fn decoder(&mut self) -> Self::Decoder {
        *self
    }
}

impl Encoder for ReadingCodec {
    type Item = ();

    type Error = Status;

    fn encode(&mut self, _item: Self::Item, _dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        // `remoteweather.v1.Empty` has no fields, and a protobuf message with no fields set
        // is zero bytes. Writing nothing is the whole encoding.
        Ok(())
    }
}

impl Decoder for ReadingCodec {
    type Item = mw_proto::wire::Reading;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        decode_reading(src).map(Some)
    }
}

/// The whole of what [`ReadingCodec`] decodes, over any buffer. [`take_all`]'s reason.
fn decode_reading(src: &mut impl Buf) -> Result<mw_proto::wire::Reading, Status> {
    // A message a satellite sent that this build cannot parse is `InvalidArgument` rather
    // than an `Internal`: the fault is in what arrived, and the sender is the only party
    // that can do anything about it.
    //
    // **It ends the stream.** `Streaming::message` surfaces a decoder error as `Err`, and
    // `grpcrx::receive` returns it, so the cost of one bad message is the rest of the
    // batch -- the same answer a refusal gets, for the same reason stated there: a sender
    // emitting messages this build cannot parse is one to stop and reconnect rather than
    // to read past. Continuing instead would be a decision about how many malformed
    // messages a stream may contain, which nothing has asked for.
    prost::Message::decode(src)
        .map_err(|e| Status::invalid_argument(format!("the reading did not decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What [`BytesCodec`] hands a driver, over a byte slice.
    fn bytes(message: &[u8]) -> Vec<u8> {
        let mut src = message;
        take_all(&mut src)
    }

    #[test]
    fn the_bytes_codec_hands_back_exactly_the_message() {
        // A snow gauge reading on the wire: field 1 `stationName`, field 2 `distance`.
        let message = [0x0a, 0x04, b'r', b'o', b'o', b'f', 0x10, 0xb0, 0x09];
        assert_eq!(bytes(&message), message);
        // An empty message is an empty frame, not an absent one -- protobuf's own
        // all-defaults case, and what `mw_codec::SnowGaugeProto` decodes to a zero distance.
        assert_eq!(bytes(&[]), Vec::<u8>::new());
    }

    #[test]
    fn the_reading_codec_decodes_a_wire_reading() {
        let reading = mw_proto::wire::Reading {
            station_id: "satellite-a:backyard".to_string(),
            barometer: Some(29.92),
            ..Default::default()
        };
        let encoded = prost::Message::encode_to_vec(&reading);
        let decoded = decode_reading(&mut &encoded[..]).expect("a well-formed reading");
        assert_eq!(decoded.station_id, "satellite-a:backyard");
        assert_eq!(decoded.barometer, Some(29.92));
        // **The presence bit**, which is D-89 and is the whole reason every metric field is
        // `optional`: a satellite that does not measure humidity is `None` here rather than
        // a reading of 0%.
        assert_eq!(decoded.outside_humidity, None);
    }

    #[test]
    fn a_message_that_is_not_a_reading_is_an_invalid_argument() {
        // A length-delimited field whose length runs off the end.
        let status = decode_reading(&mut &[0x0a, 0x40, 0x01][..]).expect_err("truncated");
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "the fault is in what arrived, so the sender is told"
        );
        assert!(status.message().contains("did not decode"), "{status}");
    }

    /// An empty message is a valid `Reading` with every field at its default, which is what
    /// the response type of `SendWeatherReadings` is on the wire -- so the encoder writing
    /// nothing is the right encoding and this is the assertion that says so.
    #[test]
    fn an_empty_message_is_a_reading_with_nothing_set() {
        let decoded = decode_reading(&mut &[][..]).expect("protobuf's all-defaults case");
        assert_eq!(decoded, mw_proto::wire::Reading::default());
    }
}
