// A Go-era satellite: `WeatherV1.SendWeatherReadings`, streaming at a collector.
//
// **The generated client is the reference tree's own**, vendored verbatim from the pinned
// commit (`remoteweather/protocols/remoteweather/`), which is the whole point of this
// emulator. `metrics/registry.toml` claims its field numbers were "bootstrapped from the Go
// implementation's own definition", and `generated/proto` is built from that registry — so
// what this test checks is that claim, on the wire, with the Go encoder on one end and the
// Rust decoder on the other. A field number that drifted would be a reading whose values
// land in the wrong columns, and **D-21** is the decision that says that must never happen.
//
// What is written in this repository is this file: the values, the cadence and the flags.
//
//	-url <authority>  the collector's host:port
//	-id <key>         the `station_id` this satellite presents -- the key it was registered
//	                  under
//	-name <name>      the `stationName` it presents; a Go-era sender sets only this
//	-count <n>        how many readings to stream
//	-interval <d>     between readings
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"math"
	"os"
	"time"

	pb "github.com/chrissnell/remoteweather/protocols/remoteweather"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"
)

func main() {
	var (
		target   = flag.String("url", "", "the collector's host:port")
		id       = flag.String("id", "satellite-a:backyard", "the `station_id` to present")
		name     = flag.String("name", "backyard", "the `stationName` to present")
		count    = flag.Int("count", 1, "how many readings to stream")
		interval = flag.Duration("interval", 20*time.Millisecond, "between readings")
	)
	flag.Parse()
	if *target == "" {
		log.Fatal("-url is required")
	}

	conn, err := grpc.NewClient(*target, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		log.Fatalf("cannot create the client: %v", err)
	}
	defer conn.Close()

	client := pb.NewWeatherV1Client(conn)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	stream, err := client.SendWeatherReadings(ctx)
	if err != nil {
		log.Fatalf("cannot open the stream: %v", err)
	}

	// A console's clock, stepped per reading, so that a receiver storing the arrival time
	// instead of the satellite's time is visible.
	observed := time.Date(2026, 11, 10, 22, 49, 9, 0, time.UTC)
	for i := 0; i < *count; i++ {
		phase := float64(i)
		reading := &pb.WeatherReading{
			ReadingTimestamp: timestamppb.New(observed),
			StationName:      *name,
			StationType:      "davis",
			StationId:        *id,
			// One value per shape the registry declares, so that a field number that
			// drifted lands somewhere the Rust side can see: a float, a second float on a
			// far-apart number, and the two that the Go schema wedged into 188-192.
			Barometer:          float32(29.92 + 0.01*phase),
			OutsideTemperature: float32(68.4 + 0.1*phase),
			OutsideHumidity:    float32(41 + math.Sin(phase)),
			WindSpeed:          float32(7.5 + 0.2*phase),
			WindDirection:      float32(183 + phase),
			RainRate:           0.02,
			SolarWatts:         float32(512.5 + phase),
			SnowDistance:       1200,
		}
		if err := stream.Send(reading); err != nil {
			// **Not fatal.** grpc-go's own rule: if `Send` returns an error the stream is
			// already over, and the REAL error is the status `CloseAndRecv` hands back --
			// `Send` reports `EOF`. A collector that refuses the first reading terminates
			// the stream, so this is the ordinary path for a refusal and not a failure of
			// this program.
			fmt.Printf("send-failed n=%d\n", i)
			os.Stdout.Sync()
			break
		}
		fmt.Printf("sent n=%d\n", i)
		os.Stdout.Sync()
		observed = observed.Add(time.Minute)
		if i+1 < *count {
			time.Sleep(*interval)
		}
	}

	// `CloseAndRecv` is what the generated client calls, and the collector answers `Empty`.
	// A refusal arrives here as the `Status` the receiver chose, which is what the test
	// asserts on.
	if _, err := stream.CloseAndRecv(); err != nil {
		fmt.Printf("refused status=%s\n", statusCode(err))
		os.Stdout.Sync()
		return
	}
	fmt.Println("accepted")
	os.Stdout.Sync()
}

// statusCode renders the gRPC code of an error, which is what the harness asserts on.
func statusCode(err error) string {
	return status.Code(err).String()
}
