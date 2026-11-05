// A module of its own, so that `go run .` works with no network access and nothing to
// resolve: this emulator imports only the standard library, on purpose.
module modernweather.test/emulators/wll

go 1.22
