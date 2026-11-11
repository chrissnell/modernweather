// A snow gauge on a gRPC port: `snowgauge.SnowGaugeService/StreamReading`, and nothing else.
//
// The **generated stubs are the reference tree's own**, vendored verbatim from the pinned
// commit (`remoteweather/protocols/snowgauge/`), so the service name, the method name and
// the message's field numbers are the ones a real gauge's firmware was generated against
// rather than ones restated here. What is written in this repository is this file: the
// values, the cadence and the flags. ../manifest.toml records `origin =
// "this-repository"` for exactly that reason -- the oracle's *protocol* is independent and
// its *behaviour* is not.
//
// Flags, each of which is a case the driver has to get right:
//
//	-port <n>        the port to listen on; 0 lets the kernel choose and the port is printed
//	-interval <d>    between readings
//	-count <n>       how many readings to send before closing the stream; 0 is forever
//	-base <mm>       the distance with no snow under it, which the daemon also has configured
//	-fault <n>       send a distance GREATER than -base every nth reading; 0 never
//	-hangup          close the stream after -count readings instead of blocking
package main

import (
	"flag"
	"fmt"
	"log"
	"math"
	"net"
	"os"
	"time"

	snowgauge "github.com/chrissnell/remoteweather/protocols/snowgauge"
	"google.golang.org/grpc"
	"google.golang.org/protobuf/types/known/durationpb"
)

type server struct {
	snowgauge.UnimplementedSnowGaugeServiceServer
	interval time.Duration
	count    int
	base     int32
	fault    int
	hangup   bool
	started  time.Time
}

// StreamReading is the one method. A real gauge reports when the distance changes; this
// reports on a timer, because a test that waited for the weather would not finish.
func (s *server) StreamReading(
	_ *snowgauge.StreamRequest,
	stream snowgauge.SnowGaugeService_StreamReadingServer,
) error {
	for n := 0; s.count == 0 || n < s.count; n++ {
		// A settling snowpack: the distance to the surface falls as snow accumulates.
		distance := s.base - int32(100+50*math.Sin(float64(n)/3))
		if s.fault > 0 && (n+1)%s.fault == 0 {
			// **The fault DEV-9 is about**: a distance GREATER than the configured base,
			// which is what a gauge reports when its sensor sees past bare ground -- and
			// which, stored, would be a NEGATIVE snow depth in a column with min/mean/max
			// aggregates over it.
			distance = s.base + 100
		}
		uptime := durationpb.New(time.Since(s.started))
		reading := &snowgauge.Reading{
			StationName:       "roof-gauge",
			Distance:          distance,
			SystemUptime:      uptime,
			ApplicationUptime: uptime,
		}
		if err := stream.Send(reading); err != nil {
			// The daemon went away, which is the ordinary end of a stream.
			log.Printf("the stream closed: %v", err)
			return nil
		}
		fmt.Printf("sent n=%d distance=%d\n", n, distance)
		os.Stdout.Sync()
		time.Sleep(s.interval)
	}
	if s.hangup {
		// The gauge closes the stream cleanly, which the driver has to treat as a station
		// that has stopped reporting rather than one that has finished.
		fmt.Println("hangup")
		os.Stdout.Sync()
		return nil
	}
	// Hold the stream open with nothing on it, which is what a gauge in settled weather
	// does.
	<-stream.Context().Done()
	return nil
}

func main() {
	var (
		port     = flag.Int("port", 0, "TCP port to listen on; 0 lets the kernel choose")
		interval = flag.Duration("interval", 50*time.Millisecond, "between readings")
		count    = flag.Int("count", 0, "readings to send; 0 is forever")
		base     = flag.Int("base", 1500, "the base distance in millimetres")
		fault    = flag.Int("fault", 0, "send a distance past -base every nth reading")
		hangup   = flag.Bool("hangup", false, "close the stream after -count readings")
	)
	flag.Parse()

	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *port))
	if err != nil {
		log.Fatalf("cannot listen: %v", err)
	}
	s := grpc.NewServer()
	snowgauge.RegisterSnowGaugeServiceServer(s, &server{
		interval: *interval,
		count:    *count,
		base:     int32(*base),
		fault:    *fault,
		hangup:   *hangup,
		started:  time.Now(),
	})

	// The one line the harness parses, because the port is the kernel's choice.
	fmt.Printf("ready grpc=%d\n", listener.Addr().(*net.TCPAddr).Port)
	os.Stdout.Sync()
	if err := s.Serve(listener); err != nil {
		log.Fatalf("the server stopped: %v", err)
	}
}
