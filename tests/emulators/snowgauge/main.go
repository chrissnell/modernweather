// A `snowgauge-rs` gauge on an HTTP port: `GET /data`, `GET /health`, and nothing else.
//
// The payload shapes are the ones `chrissnell/snowgauge-rs`'s `docs/integration.md`
// prints, key for key — the same page `mw_codec::snowgauge` and `mw_station::snowgauge`
// are written against. `origin = "this-repository"` in ../manifest.toml, because that
// firmware ships no emulator and the repository is read-only to this programme; what that
// costs is independent authorship and is recorded there, and what it keeps is a separate
// process, a different language, a real socket, a real bind, and a device that answers
// more than once with values that move.
//
// **This emulator is standard-library-only**, which its gRPC predecessor was not.
//
// Every flag is a state the driver has to get right, and each one is a line in the
// firmware's own error-handling table:
//
//	-port <n>          the port to listen on; 0 lets the kernel choose and the port is printed
//	-base <mm>         the sensor-to-ground distance the snowpack model measures against
//	-calibrated        whether the gauge has a calibration in NVS; `-calibrated=false` is a
//	                   gauge nobody has calibrated, which still MEASURES normally and
//	                   reports `base_distance` and `snow_depth` as null
//	-trim-after <n>    how many sensor frames before `trim_mean` stops being null (30 on a
//	                   real gauge, which is about a minute after boot)
//	-sensor all|dead|stalled
//	                   `dead`    — the sensor never speaks: `status` stays `no_data`,
//	                               `readings` stays 0 and `/health.bytes_rx` stays 0
//	                   `stalled` — the sensor speaks and then stops, which is the UART link
//	                               dying mid-run: `readings` freezes while `uptime_ms` grows
//	-stall-after <n>   how many frames `-sensor stalled` produces first
//	-fault <n>         report a distance GREATER than -base every nth response; 0 never
//	-reboot <n>        reboot every nth response: `uptime_ms` drops and `readings` restarts
//	-uptime-step <ms>  how much `uptime_ms` advances per response (see `tick`)
//	-firmware current|old
//	                   `old` is a gauge that predates `/data`: 404 there, and `/health`
//	                   reports a version before the endpoint existed
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"math"
	"net"
	"net/http"
	"os"
	"sync"
	"time"
)

// Data is `GET /data`. `TrimMean` is a pointer because it is `null` until the trailing
// window has filled, which is the field's whole subtlety: a non-pointer `float64` would
// serialise a zero and tell the daemon the snowpack had reached the sensor.
type Data struct {
	Distance     uint16   `json:"distance"`
	Status       string   `json:"status"`
	Readings     uint64   `json:"readings"`
	TrimMean     *float64 `json:"trim_mean"`
	BaseDistance *uint16  `json:"base_distance"`
	SnowDepth    *int32   `json:"snow_depth"`
	RSSI         int32    `json:"rssi"`
	UptimeMs     uint64   `json:"uptime_ms"`
}

// Health is `GET /health`.
type Health struct {
	WiFi      string `json:"wifi"`
	IP        string `json:"ip"`
	RSSI      int    `json:"rssi"`
	Hostname  string `json:"hostname"`
	BytesRx   uint64 `json:"bytes_rx"`
	HeapFree  uint64 `json:"heap_free"`
	FwVersion string `json:"fw_version"`
	FwSlot    string `json:"fw_slot"`
}

// gauge is the device's state. One mutex rather than atomics: a reboot moves three fields
// together, and a response that saw two of the three would be a state no gauge is ever in.
type gauge struct {
	mu sync.Mutex

	served    uint64
	sinceBoot uint64
	readings  uint64

	base       float64
	trimAfter  uint64
	sensor     string
	stallAfter uint64
	fault      uint64
	reboot     uint64
	uptimeStep uint64
	calibrated bool
}

// tick advances the device by one `/data` request and returns what it would answer.
//
// `uptime_ms` is advanced one `-uptime-step` per response rather than read off the wall
// clock, and that is deliberate: a real gauge is polled seconds apart and this one answers
// in microseconds, so a wall-clock uptime would give several consecutive responses the same
// millisecond -- and a reboot, which the driver detects as `uptime_ms` DECREASING, would be
// invisible between two responses that both read zero. The step is the daemon's own default
// poll period, so what the driver sees is what it would see from a device it was polling.
func (g *gauge) tick() Data {
	g.mu.Lock()
	defer g.mu.Unlock()

	g.served++
	g.sinceBoot++
	if g.reboot > 0 && g.served%g.reboot == 0 {
		// Both counters restart, which is the case a driver that tested the sensor before
		// the uptime would report as a dead sensor on every reboot.
		g.readings = 0
		g.sinceBoot = 1
	}

	switch g.sensor {
	case "dead":
		// Never anything.
	case "stalled":
		if g.readings < g.stallAfter {
			g.readings++
		}
	default:
		g.readings++
	}

	d := Data{
		Status:   "no_data",
		Readings: g.readings,
		UptimeMs: g.sinceBoot * g.uptimeStep,
	}

	// **Set before the early return, because neither comes from the sensor.** `rssi` is the
	// WiFi driver's and `base_distance` is NVS's, so the firmware reports both whatever the
	// sensor is doing -- a calibrated gauge with a dead UART still tells you what it is
	// calibrated to, which is the whole point of reading the number off the device. Never
	// null on the wire: the firmware's `sta_rssi()` returns an i32 and yields 0 when the AP
	// lookup fails.
	d.RSSI = -63
	if g.calibrated {
		base := uint16(math.Round(g.base))
		d.BaseDistance = &base
	}

	if g.readings == 0 {
		// `distance` is 0 and `status` is `no_data` until the first frame arrives, which
		// is the firmware's documented startup state and not a measurement of zero. The
		// DEPTH is null here even on a calibrated gauge, which is the firmware's own
		// `(Some(b), None, "no_data") => null` arm.
		return d
	}
	d.Status = "ok"

	// A settling snowpack: the distance to the surface falls as snow accumulates.
	distance := g.base - (100 + 50*math.Sin(float64(g.readings)/3))
	trimmed := distance
	if g.fault > 0 && g.served%g.fault == 0 {
		// **The fault DEV-9 is about**: a distance GREATER than the configured base, which
		// is what a gauge reports when its sensor sees past bare ground -- and which,
		// stored, would be a NEGATIVE snow depth in a column with min/mean/max aggregates
		// over it.
		distance = g.base + 100
		trimmed = distance
	}
	d.Distance = uint16(math.Round(distance))
	if g.readings >= g.trimAfter {
		// One decimal, which is what the firmware's `format!("{v:.1}")` writes.
		rounded := math.Round(trimmed*10) / 10
		d.TrimMean = &rounded
	}

	// **The device's own depth.**
	if g.calibrated {
		base := uint16(math.Round(g.base))
		// `base as i32 - reading.round() as i32`, in that order, which is the firmware's
		// arithmetic verbatim (`server.rs`: `(b as i32 - m.round() as i32)`). Rounding the
		// SUBTRACTION instead would differ by up to a millimetre, and an oracle that is a
		// millimetre off the device is an oracle that hides a millimetre of drift.
		// The **unrounded** trimmed mean, which is what the firmware's own `m` is: `/data`
		// formats it to one decimal for the wire and computes the depth from the f32. Taking
		// the formatted value back would differ by a millimetre wherever one-decimal
		// rounding crosses a half, and an oracle a millimetre off the device hides a
		// millimetre of drift.
		reading := float64(d.Distance)
		if d.TrimMean != nil {
			reading = trimmed
		}
		depth := int32(base) - int32(math.Round(reading))
		d.SnowDepth = &depth
	}
	return d
}

func (g *gauge) health(firmware string) Health {
	g.mu.Lock()
	defer g.mu.Unlock()
	version := "0.1.0"
	if firmware == "old" {
		version = "0.0.9"
	}
	return Health{
		WiFi:     "connected",
		IP:       "127.0.0.1",
		RSSI:     -56,
		Hostname: "snowgauge-f878",
		// The cross-check the firmware's own error table names: zero bytes is a sensor
		// that is unpowered or unwired, and this is where `-sensor dead` is visible as
		// something other than a missing number.
		BytesRx:   g.readings * 8,
		HeapFree:  164228,
		FwVersion: version,
		FwSlot:    "ota_0",
	}
}

func main() {
	var (
		port       = flag.Int("port", 0, "TCP port to listen on; 0 lets the kernel choose")
		base       = flag.Float64("base", 1500, "sensor-to-ground distance in mm")
		calibrated = flag.Bool("calibrated", true, "whether the gauge has a calibration in NVS")
		trimAfter  = flag.Uint64("trim-after", 30, "sensor frames before `trim_mean` lands")
		sensor     = flag.String("sensor", "all", "`all`, `dead` or `stalled`")
		stallAfter = flag.Uint64("stall-after", 5, "frames before `-sensor stalled` stops")
		fault      = flag.Uint64("fault", 0, "report a distance past -base every nth response")
		reboot     = flag.Uint64("reboot", 0, "reboot every nth response; 0 never")
		uptimeStep = flag.Uint64("uptime-step", 5000, "ms `uptime_ms` advances per response")
		firmware   = flag.String("firmware", "current", "`current` or `old`")
	)
	flag.Parse()

	switch *sensor {
	case "all", "dead", "stalled":
	default:
		log.Fatalf("-sensor must be `all`, `dead` or `stalled`, not %q", *sensor)
	}

	// `distance` and `base_distance` are u16 millimetres on the wire, and the snowpack model
	// subtracts up to 150 mm from the base. A base outside this range would silently wrap
	// the uint16 conversion in `tick` and hand the driver a reading no device can produce --
	// which is how a test asserting the wrong thing gets written and believed.
	if *base < 200 || *base > 65535 {
		log.Fatalf("-base must be between 200 and 65535 mm (u16 on the wire), not %v", *base)
	}

	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *port))
	if err != nil {
		log.Fatalf("cannot listen: %v", err)
	}

	g := &gauge{
		base:       *base,
		trimAfter:  *trimAfter,
		sensor:     *sensor,
		stallAfter: *stallAfter,
		fault:      *fault,
		reboot:     *reboot,
		uptimeStep: *uptimeStep,
		calibrated: *calibrated,
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/data", func(w http.ResponseWriter, r *http.Request) {
		if *firmware == "old" {
			// A gauge that predates the endpoint. Its own documentation says this means
			// firmware whose only reading surface was the `/events` SSE stream, removed in
			// v0.1.x -- so the driver has to say "upgrade" rather than "not found".
			http.NotFound(w, r)
			return
		}
		write(w, g.tick())
	})
	mux.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		write(w, g.health(*firmware))
	})

	// The one line the harness parses, on stdout and flushed, because the port is the
	// kernel's choice and nothing else can tell the test what it was.
	addr := listener.Addr().(*net.TCPAddr)
	fmt.Printf("ready http=%d\n", addr.Port)
	os.Stdout.Sync()

	server := &http.Server{Handler: mux, ReadHeaderTimeout: 5 * time.Second}
	if err := server.Serve(listener); err != nil {
		log.Fatalf("the server stopped: %v", err)
	}
}

func write(w http.ResponseWriter, body any) {
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(body); err != nil {
		log.Printf("cannot write the response: %v", err)
	}
}
