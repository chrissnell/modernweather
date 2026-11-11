// An Ambient Weather console in "customized" mode: the station is the **client**.
//
// This is the one emulator in this tree that connects outwards, and that is the whole
// shape of the Ambient row in docs/component-model.md §4a.5. ../manifest.toml's pending
// entry said so before this existed: *"the station is the client, so the test is a request
// against the route the driver mounts"*. So there is no socket here to bind and no port to
// report; what this does is send the vendor's own `updateraw` query string at a URL, N
// times, and print what it was answered.
//
// `origin = "this-repository"`. The reference tree has no Ambient emulator at the pinned
// commit and is read-only to this programme, so this one is written here and
// ../manifest.toml records what that costs. What it keeps is the properties that matter at
// this level: a separate process, a different language, a real HTTP request over a real
// socket at the real route, measurements that move, and a monotonic rainfall counter --
// which is the one piece of state the driver holds.
//
// The parameter set is `internal/weatherstations/ambientcustomized/station.go`'s, which is
// the vendor's "customized" vocabulary: `ID`, `PASSWORD`, `action=updateraw`, `dateutc`,
// and the measurements a WS-2902 with an air-quality module and a lightning detector
// sends.
//
// Modes, each of which is a case the endpoint has to get right:
//
//	-mode normal      a well-formed upload
//	-mode no-dateutc  the one required parameter a console can lose, which is a 400
//	-mode no-action   the same, for `action=updateraw`
//	-mode bad-password a console whose operator typed the secret wrongly, which is a 401
package main

import (
	"flag"
	"fmt"
	"io"
	"log"
	"math"
	"net/http"
	"net/url"
	"os"
	"time"
)

func main() {
	var (
		endpoint = flag.String("url", "", "the ingest endpoint, including its path")
		id       = flag.String("id", "roof", "the `?ID=` this console presents")
		password = flag.String("password", "hunter2", "the `?PASSWORD=` it presents")
		count    = flag.Int("count", 1, "how many uploads to send")
		interval = flag.Duration("interval", 50*time.Millisecond, "between uploads")
		mode     = flag.String("mode", "normal", "normal|no-dateutc|no-action|bad-password")
		rainFrom = flag.Float64("total-rain", 118.70, "the all-time rainfall counter's start")
	)
	flag.Parse()
	if *endpoint == "" {
		log.Fatal("-url is required")
	}

	client := &http.Client{Timeout: 10 * time.Second}
	// A console's clock, which is what `dateutc` carries. Fixed and then stepped, so that
	// a driver storing the arrival time instead of the station's time is visible.
	observed := time.Date(2026, 11, 10, 22, 49, 9, 0, time.UTC)

	for i := 0; i < *count; i++ {
		q := url.Values{}
		q.Set("ID", *id)
		if *mode == "bad-password" {
			q.Set("PASSWORD", *password+"-wrong")
		} else {
			q.Set("PASSWORD", *password)
		}
		if *mode != "no-action" {
			q.Set("action", "updateraw")
		}
		if *mode != "no-dateutc" {
			// The vendor's format, which is not RFC 3339 and is what the Go handler parses.
			q.Set("dateutc", observed.Format("2006-01-02 15:04:05"))
		}

		phase := float64(i)
		// The console. Every value moves, so a driver that admitted the first upload and
		// then stopped shows up as a repeated number rather than a missing one.
		set(q, "tempf", 68.4+0.1*phase)
		set(q, "humidity", 41+math.Sin(phase))
		set(q, "dewptf", 44.2+0.1*phase)
		set(q, "winddir", 183+phase)
		set(q, "windspeedmph", 7.5+0.2*phase)
		set(q, "windgustmph", 12.1+0.2*phase)
		set(q, "baromin", 30.12)
		set(q, "baromrelin", 30.12)
		set(q, "baromabsin", 29.88)
		set(q, "solarradiation", 512.5+phase)
		set(q, "UV", 3)
		set(q, "indoortempf", 70.1)
		set(q, "indoorhumidity", 38)
		// Rain. `rainin` is the vendor's HOURLY figure and `totalrainin` is the all-time
		// counter the driver differences -- 0.05 in per upload, so the increment is a known
		// number rather than one the test has to re-derive.
		set(q, "rainin", 0.02)
		set(q, "dailyrainin", 0.31)
		set(q, "weeklyrainin", 1.10)
		set(q, "monthlyrainin", 2.40)
		set(q, "totalrainin", *rainFrom+0.05*phase)
		// The air-quality module and the lightning detector.
		set(q, "pm25", 8.1+0.1*phase)
		set(q, "pm25_24h", 9.4)
		set(q, "co2", 612)
		set(q, "aqi_pm25_aqin", 34)
		set(q, "lightning_day", 3)
		set(q, "lightning_distance", 8.5)
		q.Set("lightning_time", observed.Format(time.RFC3339))
		q.Set("tz", "America/Denver")
		set(q, "battout", 1)
		set(q, "lowbatt", 0)

		request := *endpoint + "?" + q.Encode()
		response, err := client.Get(request)
		if err != nil {
			log.Fatalf("upload %d failed: %v", i, err)
		}
		// Read to EOF before closing, which is what lets the transport reuse the
		// connection -- a `Close` on an undrained body does not, so the endpoint would
		// see N clients rather than one. The body is also what the console's own firmware
		// keys off, so it is reported rather than discarded.
		body, err := io.ReadAll(response.Body)
		if err != nil {
			log.Fatalf("upload %d: reading the response: %v", i, err)
		}
		_ = response.Body.Close()
		fmt.Printf("upload n=%d status=%d body=%q\n", i, response.StatusCode, string(body))
		os.Stdout.Sync()

		observed = observed.Add(time.Minute)
		if i+1 < *count {
			time.Sleep(*interval)
		}
	}
}

// set writes one measurement, in the shortest decimal form that round-trips -- which is
// what a console's own firmware does and what keeps a decoded `f32` comparable against the
// number on the wire.
func set(q url.Values, key string, value float64) {
	q.Set(key, fmt.Sprintf("%g", math.Round(value*100)/100))
}
