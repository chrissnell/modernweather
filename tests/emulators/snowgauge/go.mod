// A module of its own, so that `go build .` works with no network access and nothing to
// resolve: this emulator imports only the standard library, on purpose.
//
// **It did not use to.** The gauge spoke gRPC, so this directory needed
// `google.golang.org/grpc`, a committed `go.sum`, and the reference tree's generated
// protobuf stubs vendored beside it — which made it one of the two emulators here that
// `go build` could not produce offline. The firmware serves JSON over plain HTTP now, so
// `net/http` is the whole of it and the stubs, the module requirements and the digests in
// ../manifest.toml are gone with them.
module modernweather.test/emulators/snowgauge

go 1.22
