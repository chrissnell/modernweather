// A WeatherLink Live emulator: the local API's two endpoints and its UDP broadcast.
//
// WHY THIS IS HERE AND NOT IN THE REFERENCE TREE, which is where DESIGN.md D-40 says the
// emulators live. The reference repository at the commit tests/emulators/manifest.toml
// pins has four emulators -- davis-emulator, campbell-emulator, weather-station-simulator
// and live-data-simulator -- and **none of them emulates a WeatherLink Live**. The
// repository is read-only to this programme, so the only way to arm the driver-emulators
// gate with something that actually runs is to write this one here. The manifest records
// what that costs, in the same words: this emulator is not an independently-written
// oracle, because it was written in the same repository as the driver it drives. What it
// still buys is every other property the level is for -- a separate process, a different
// language, real sockets, and a driver exercised for more than one batch.
//
// It is written from the published WeatherLink Live local API v1, cross-checked against
// the field set internal/weatherstations/weatherlinklive/types.go declares in the Go
// reference tree. Standard library only, so `go run .` needs no network.
//
// Usage:
//
//	go run . -http-port 8080 -udp-port 22222 -udp-host 127.0.0.1 -interval 200ms
//
// It prints one line, `ready http=<port> udp=<port>`, once both sockets are up, so the
// harness does not have to guess.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"math"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"sync"
	"syscall"
	"time"
)

// The serial this unit reports, in the case a real one reports it: upper-case hex.
const defaultSerial = "001D0A700002"

// The rain collector: 1 = 0.01 in per tip, which is the US default.
const rainSize = 1

type emulator struct {
	serial  string
	started time.Time
	mu      sync.Mutex
	// Where the broadcast goes, and until when. Zero time means "not armed", which is the
	// state a real unit is in until something calls /v1/real_time.
	armedTo  time.Time
	udpAddr  *net.UDPAddr
	udpPort  int
	conn     *net.UDPConn
	requests map[string]int
}

// condition is one `conditions` record. Every field is a pointer so that an absent sensor
// serialises as `null`, which is what a real unit does and what the decoder under test has
// to tolerate.
type condition struct {
	Lsid              int  `json:"lsid"`
	DataStructureType int  `json:"data_structure_type"`
	TxID              *int `json:"txid,omitempty"`

	// ISS
	Temp                  *float64 `json:"temp,omitempty"`
	Hum                   *float64 `json:"hum,omitempty"`
	DewPoint              *float64 `json:"dew_point,omitempty"`
	WetBulb               *float64 `json:"wet_bulb,omitempty"`
	HeatIndex             *float64 `json:"heat_index,omitempty"`
	WindChill             *float64 `json:"wind_chill,omitempty"`
	THWIndex              *float64 `json:"thw_index,omitempty"`
	THSWIndex             *float64 `json:"thsw_index,omitempty"`
	WindSpeedLast         *float64 `json:"wind_speed_last,omitempty"`
	WindDirLast           *int     `json:"wind_dir_last,omitempty"`
	WindSpeedAvgLast10Min *float64 `json:"wind_speed_avg_last_10_min,omitempty"`
	RainSize              *int     `json:"rain_size,omitempty"`
	RainRateLast          *float64 `json:"rain_rate_last,omitempty"`
	RainfallDaily         *float64 `json:"rainfall_daily,omitempty"`
	SolarRad              *int     `json:"solar_rad,omitempty"`
	UVIndex               *float64 `json:"uv_index,omitempty"`
	RxState               *int     `json:"rx_state,omitempty"`
	TransBatteryFlag      *int     `json:"trans_battery_flag,omitempty"`

	// Barometer (type 3)
	BarSeaLevel *float64 `json:"bar_sea_level,omitempty"`
	BarTrend    *float64 `json:"bar_trend,omitempty"`
	BarAbsolute *float64 `json:"bar_absolute,omitempty"`

	// The unit's own temp/humidity (type 4)
	TempIn      *float64 `json:"temp_in,omitempty"`
	HumIn       *float64 `json:"hum_in,omitempty"`
	DewPointIn  *float64 `json:"dew_point_in,omitempty"`
	HeatIndexIn *float64 `json:"heat_index_in,omitempty"`
}

type document struct {
	DID        string      `json:"did"`
	TS         int64       `json:"ts"`
	Conditions []condition `json:"conditions"`
}

type apiError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type conditionsResponse struct {
	Data  document  `json:"data"`
	Error *apiError `json:"error"`
}

type realTimeData struct {
	BroadcastPort int `json:"broadcast_port"`
	Duration      int `json:"duration"`
}

type realTimeResponse struct {
	Data  realTimeData `json:"data"`
	Error *apiError    `json:"error"`
}

func f(v float64) *float64 { return &v }
func i(v int) *int         { return &v }

// iss builds the Integrated Sensor Suite record. The values move, so a driver that
// admitted one reading and then stopped is visibly different from one that keeps up.
func (e *emulator) iss(now time.Time) condition {
	phase := now.Sub(e.started).Seconds()
	temp := 62.0 + 3*math.Sin(phase/10)
	wind := 4.0 + 2*math.Sin(phase/7)
	return condition{
		Lsid:                  48308,
		DataStructureType:     1,
		TxID:                  i(1),
		Temp:                  f(round(temp)),
		Hum:                   f(round(52.0 + 5*math.Cos(phase/13))),
		DewPoint:              f(round(temp - 15)),
		WetBulb:               f(round(temp - 9)),
		HeatIndex:             f(round(temp)),
		WindChill:             f(round(temp - 1)),
		THWIndex:              f(round(temp - 1)),
		THSWIndex:             f(round(temp + 2)),
		WindSpeedLast:         f(round(wind)),
		WindDirLast:           i(int(280 + 20*math.Sin(phase/5))),
		WindSpeedAvgLast10Min: f(round(wind - 0.5)),
		RainSize:              i(rainSize),
		RainRateLast:          f(0),
		RainfallDaily:         f(12),
		SolarRad:              i(int(700 + 40*math.Sin(phase/11))),
		UVIndex:               f(2.7),
		RxState:               i(0),
		TransBatteryFlag:      i(0),
	}
}

// full is what `GET /v1/current_conditions` answers: the ISS, the unit's own
// temp/humidity, and the unit's barometer.
func (e *emulator) full(now time.Time) document {
	phase := now.Sub(e.started).Seconds()
	return document{
		DID: e.serial,
		TS:  now.Unix(),
		Conditions: []condition{
			e.iss(now),
			{
				Lsid:              48307,
				DataStructureType: 4,
				TempIn:            f(round(78.0 + math.Sin(phase/17))),
				HumIn:             f(41.1),
				DewPointIn:        f(52.5),
				HeatIndexIn:       f(77.0),
			},
			{
				Lsid:              48306,
				DataStructureType: 3,
				BarSeaLevel:       f(round2(29.897 + 0.01*math.Sin(phase/23))),
				BarTrend:          f(0),
				BarAbsolute:       f(29.846),
			},
		},
	}
}

// broadcast is what the UDP datagram carries: one ISS record with the high-rate wind and
// rain fields, and NOT wrapped in `data`. That asymmetry between the two transports is the
// thing most worth emulating, because it is the thing a decoder gets wrong.
func (e *emulator) broadcast(now time.Time) document {
	iss := e.iss(now)
	// The broadcast record carries no temperature, humidity or indices.
	iss.Temp = nil
	iss.Hum = nil
	iss.DewPoint = nil
	iss.WetBulb = nil
	iss.HeatIndex = nil
	iss.WindChill = nil
	iss.THWIndex = nil
	iss.THSWIndex = nil
	iss.SolarRad = nil
	iss.UVIndex = nil
	return document{DID: e.serial, TS: now.Unix(), Conditions: []condition{iss}}
}

func round(v float64) float64  { return math.Round(v*10) / 10 }
func round2(v float64) float64 { return math.Round(v*1000) / 1000 }

func (e *emulator) count(path string) int {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.requests[path]++
	return e.requests[path]
}

func (e *emulator) handleConditions(w http.ResponseWriter, r *http.Request) {
	e.count("/v1/current_conditions")
	writeJSON(w, conditionsResponse{Data: e.full(time.Now())})
}

func (e *emulator) handleRealTime(w http.ResponseWriter, r *http.Request) {
	n := e.count("/v1/real_time")
	duration := 3600
	if raw := r.URL.Query().Get("duration"); raw != "" {
		if parsed, err := strconv.Atoi(raw); err == nil && parsed > 0 {
			duration = parsed
		}
	}
	// A real unit caps the duration at its own maximum and reports what it granted. The
	// cap is what makes the driver's "granted less than my period" warning reachable.
	if duration > 3600 {
		duration = 3600
	}
	// Every fifth arming is refused with a 200 and a populated `error`, which is what a
	// unit does when another client already holds the broadcast. A client that read the
	// status line and not the body would treat it as success and listen to silence.
	if n%5 == 0 {
		writeJSON(w, realTimeResponse{Error: &apiError{Code: 409, Message: "Request Rejected"}})
		return
	}
	e.mu.Lock()
	e.armedTo = time.Now().Add(time.Duration(duration) * time.Second)
	e.mu.Unlock()
	writeJSON(w, realTimeResponse{Data: realTimeData{BroadcastPort: e.udpPort, Duration: duration}})
}

func (e *emulator) armed() bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	return time.Now().Before(e.armedTo)
}

func writeJSON(w http.ResponseWriter, body any) {
	w.Header().Set("Content-Type", "application/json")
	encoded, err := json.Marshal(body)
	if err != nil {
		http.Error(w, "encode", http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Length", strconv.Itoa(len(encoded)))
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(encoded)
}

func main() {
	httpPort := flag.Int("http-port", 0, "port for the local API; 0 picks a free one")
	udpPort := flag.Int("udp-port", 0, "port to broadcast conditions to; 0 picks a free one")
	udpHost := flag.String("udp-host", "127.0.0.1", "where to send the datagrams")
	interval := flag.Duration("interval", 200*time.Millisecond, "datagram interval")
	serial := flag.String("serial", defaultSerial, "the device serial to report")
	alwaysBroadcast := flag.Bool("always-broadcast", false,
		"broadcast without waiting to be armed, for a listener-only test")
	flag.Parse()

	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *httpPort))
	if err != nil {
		fmt.Fprintf(os.Stderr, "wll-emulator: cannot listen: %v\n", err)
		os.Exit(1)
	}
	resolvedHTTP := listener.Addr().(*net.TCPAddr).Port

	// A sending socket, and the port to send to. Port 0 means the harness did not care, so
	// one is chosen and reported -- which is what keeps two emulators on one machine from
	// colliding.
	target := *udpPort
	if target == 0 {
		probe, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			fmt.Fprintf(os.Stderr, "wll-emulator: cannot pick a UDP port: %v\n", err)
			os.Exit(1)
		}
		target = probe.LocalAddr().(*net.UDPAddr).Port
		_ = probe.Close()
	}
	conn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		fmt.Fprintf(os.Stderr, "wll-emulator: cannot open a sending socket: %v\n", err)
		os.Exit(1)
	}

	e := &emulator{
		serial:   *serial,
		started:  time.Now(),
		udpPort:  target,
		udpAddr:  &net.UDPAddr{IP: net.ParseIP(*udpHost), Port: target},
		conn:     conn,
		requests: map[string]int{},
	}
	if *alwaysBroadcast {
		e.armedTo = time.Now().Add(24 * time.Hour)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/v1/current_conditions", e.handleConditions)
	mux.HandleFunc("/v1/real_time", e.handleRealTime)
	server := &http.Server{Handler: mux, ReadHeaderTimeout: 5 * time.Second}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		ticker := time.NewTicker(*interval)
		defer ticker.Stop()
		for {
			select {
			case <-stop:
				return
			case now := <-ticker.C:
				if !e.armed() {
					continue
				}
				encoded, err := json.Marshal(e.broadcast(now))
				if err != nil {
					continue
				}
				_, _ = e.conn.WriteToUDP(encoded, e.udpAddr)
			}
		}
	}()

	// One line, after both sockets exist, so the harness waits on a fact rather than on a
	// duration.
	fmt.Printf("ready http=%d udp=%d\n", resolvedHTTP, target)
	os.Stdout.Sync()

	go func() {
		<-stop
		_ = server.Close()
		_ = e.conn.Close()
	}()
	if err := server.Serve(listener); err != nil && err != http.ErrServerClosed {
		fmt.Fprintf(os.Stderr, "wll-emulator: %v\n", err)
		os.Exit(1)
	}
}
