// A module of its own, so that the emulator builds with no network access and nothing to
// resolve: `main.go` is verbatim from the pinned reference commit and imports only the
// standard library. See README.md.
module modernweather.test/emulators/campbell

go 1.22
