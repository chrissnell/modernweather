// A module of its own, so that `go build .` works with no network access and nothing to
// resolve: this emulator imports only the standard library, on purpose.
module modernweather.test/emulators/ambient

go 1.22
