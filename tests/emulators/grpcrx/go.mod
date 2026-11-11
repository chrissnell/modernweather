// A module of its own, with a `replace` so that `main.go` imports the vendored generated
// stubs under the path they were generated for.
//
// **Not standard-library-only**: it speaks gRPC, so `go build` needs a populated module
// cache or the network. ../manifest.toml records that; README.md has why it is worth it.
module modernweather.test/emulators/grpcrx

go 1.22

require (
	github.com/chrissnell/remoteweather v0.0.0
	google.golang.org/grpc v1.70.0
	google.golang.org/protobuf v1.36.5
)

require (
	golang.org/x/net v0.33.0 // indirect
	golang.org/x/sys v0.28.0 // indirect
	golang.org/x/text v0.21.0 // indirect
	google.golang.org/genproto/googleapis/api v0.0.0-20250106144421-5f5ef82da422 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20250102185135-69823020774d // indirect
)

replace github.com/chrissnell/remoteweather => ./remoteweather
