// The pinned reference repository's own module path, so that the vendored generated stubs
// resolve under the import path they were generated for -- with no edit to either file.
module github.com/chrissnell/remoteweather

go 1.22

require (
	google.golang.org/genproto/googleapis/api v0.0.0-20250106144421-5f5ef82da422
	google.golang.org/grpc v1.70.0
	google.golang.org/protobuf v1.36.5
)
