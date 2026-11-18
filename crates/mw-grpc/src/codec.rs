//! The `tonic::codec::Codec` implementation this crate's service uses, and why it is not a
//! generated one.
//!
//! A `tonic` service is normally generated from a `.proto` by `tonic-prost-build`, which
//! emits a client, a server trait, and a `ProstCodec` over message types it also emits.
//!
//! **There used to be two codecs here**, and the second was `BytesCodec`: a codec that did
//! not decode, handing the snow gauge's message bytes straight to `mw-codec`'s pure decoder
//! so that there was one declaration of `snowgauge.Reading` rather than two. The gauge's
//! firmware serves JSON over plain HTTP now (`mw_station::snowgauge`), so there is no
//! outbound gRPC call in this build and nothing that needs a byte-passthrough codec.
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
//! service name, so a typo in it is a service that compiles and answers `Unimplemented`.
//! It is declared beside the schema it belongs to ([`crate::grpcrx::SEND_WEATHER_READINGS`])
//! and has a test asserting the exact string, because that string is a wire contract.

use prost::bytes::Buf;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::Status;

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

/// The whole of what [`ReadingCodec`] decodes, over any buffer.
///
/// A free function because `tonic::codec::DecodeBuf` has no public constructor, so the only
/// way to test a `Decoder` impl is to run a gRPC call — which is the emulator level, not
/// this one. Factoring the decision out means level 1 can assert it over a byte slice,
/// which is what every other decoder in this workspace is tested as.
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
