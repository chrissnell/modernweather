// An AirGradient ONE on the LAN: `GET /measures/current`, and nothing else.
//
// `origin = "this-repository"` in ../manifest.toml, because the reference tree has no
// AirGradient emulator at the pinned commit and the repository is read-only to this
// programme. What that costs is independent authorship and is recorded there; what it
// keeps is a separate process, a different language, a real socket, a real bind, and a
// device that answers more than once with values that move.
//
// The payload is the device's published `/measures/current` shape, which is also the one
// `internal/weatherstations/airgradient/station.go`'s response struct decodes, field for
// field. Three flags make the two cases a driver has to get right reachable:
//
//	-firmware old        omit every `*Compensated` field, which is what an older firmware
//	                     does -- and what makes the Go driver store 32 degF
//	-sensor none         omit the temperature and humidity entirely
//	-status <code>       answer that status instead of 200, with no body
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"math"
	"math/rand"
	"net"
	"net/http"
	"os"
	"sync/atomic"
	"time"
)

// Measures is the device's `/measures/current` payload. Pointers, so that a field this
// firmware does not report is absent from the JSON rather than zero -- which is the whole
// point of the `-firmware old` and `-sensor none` modes.
type Measures struct {
	PM01            *float64 `json:"pm01,omitempty"`
	PM02            *float64 `json:"pm02,omitempty"`
	PM10            *float64 `json:"pm10,omitempty"`
	PM02Compensated *float64 `json:"pm02Compensated,omitempty"`
	PM003Count      *float64 `json:"pm003Count,omitempty"`
	PM005Count      *float64 `json:"pm005Count,omitempty"`
	PM01Count       *float64 `json:"pm01Count,omitempty"`
	PM02Count       *float64 `json:"pm02Count,omitempty"`
	Atmp            *float64 `json:"atmp,omitempty"`
	AtmpCompensated *float64 `json:"atmpCompensated,omitempty"`
	Rhum            *float64 `json:"rhum,omitempty"`
	RhumCompensated *float64 `json:"rhumCompensated,omitempty"`
	Rco2            *float64 `json:"rco2,omitempty"`
	TvocIndex       *float64 `json:"tvocIndex,omitempty"`
	TvocRaw         *float64 `json:"tvocRaw,omitempty"`
	NoxIndex        *float64 `json:"noxIndex,omitempty"`
	NoxRaw          *float64 `json:"noxRaw,omitempty"`
	Wifi            *float64 `json:"wifi,omitempty"`
	SerialNo        string   `json:"serialno,omitempty"`
	Firmware        string   `json:"firmware,omitempty"`
	Model           string   `json:"model,omitempty"`
}

func f(v float64) *float64 { return &v }

func main() {
	var (
		port     = flag.Int("port", 0, "TCP port to listen on; 0 lets the kernel choose")
		firmware = flag.String("firmware", "current", "`current` or `old`")
		sensor   = flag.String("sensor", "all", "`all` or `none`")
		status   = flag.Int("status", http.StatusOK, "the status to answer with")
		serial   = flag.String("serial", "ecda3bbd1f2c", "the device serial number")
	)
	flag.Parse()

	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *port))
	if err != nil {
		log.Fatalf("cannot listen: %v", err)
	}

	// Every response moves, so a driver that answered the first body and then stopped is
	// visible as a repeated value rather than as a missing one.
	var served int64
	mux := http.NewServeMux()
	mux.HandleFunc("/measures/current", func(w http.ResponseWriter, r *http.Request) {
		n := atomic.AddInt64(&served, 1)
		if *status != http.StatusOK {
			w.WriteHeader(*status)
			return
		}
		phase := float64(n)
		m := Measures{
			PM01:       f(round(1.0 + 0.1*math.Sin(phase))),
			PM02:       f(round(3.0 + 0.2*math.Sin(phase))),
			PM10:       f(round(4.0 + 0.3*math.Sin(phase))),
			PM003Count: f(410 + float64(n)),
			PM005Count: f(120 + float64(n)),
			PM01Count:  f(33 + float64(n)),
			PM02Count:  f(4 + float64(n)),
			Rco2:       f(round(600 + 20*math.Sin(phase) + 5*rand.Float64())),
			TvocIndex:  f(round(100 + 5*math.Sin(phase))),
			TvocRaw:    f(31000),
			NoxIndex:   f(1),
			NoxRaw:     f(16000),
			Wifi:       f(-58),
			SerialNo:   *serial,
			Firmware:   "3.1.9",
			Model:      "I-9PSL",
		}
		if *sensor != "none" {
			// The raw pair is what every firmware reports.
			m.Atmp = f(round(21.0 + 0.5*math.Sin(phase)))
			m.Rhum = f(round(44.0 + math.Sin(phase)))
			if *firmware != "old" {
				// The compensated pair, which only current firmware adds -- and which the
				// Go driver reads *instead of* the raw one, storing 32 degF when it is
				// absent.
				m.AtmpCompensated = f(round(*m.Atmp - 1.5))
				m.RhumCompensated = f(round(*m.Rhum + 2.0))
				m.PM02Compensated = f(round(*m.PM02 - 0.5))
			}
		}
		w.Header().Set("Content-Type", "application/json")
		if err := json.NewEncoder(w).Encode(m); err != nil {
			log.Printf("cannot write the response: %v", err)
		}
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

// round to two decimals, so that a decoded `f32` compares exactly against the number in
// the JSON rather than against a value with a longer tail than the wire had.
func round(v float64) float64 {
	return math.Round(v*100) / 100
}
