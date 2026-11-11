// A module of its own, so that the emulator builds with no network access and nothing to
// resolve.
//
// `main.go` is **verbatim** from the pinned reference commit and imports
// `github.com/chrissnell/remoteweather/pkg/crc16`, so that import has to resolve without
// the file being edited -- which is what the `replace` below is for. `crc16.go` under
// `remoteweather/` is verbatim from the same commit. See README.md.
module modernweather.test/emulators/davis

go 1.22

require github.com/chrissnell/remoteweather v0.0.0

replace github.com/chrissnell/remoteweather => ./remoteweather
