package companion

import (
	"encoding/hex"
	"sync"

	"github.com/wilbowes/EchoMuse/pkg/led"
)

// NumLEDs is the bar's segment count: the ring's, index 0 at the left.
const NumLEDs = 12

// Sink is the ring compositor's led.Controller on the LineageOS Show (plan
// decision 7): it keeps the last painted frame and tells the server, which
// sends it to the app.
type Sink struct {
	mu      sync.Mutex
	rgb     [NumLEDs * 3]byte
	changed func()
}

func (*Sink) Init() error { return nil }

func (*Sink) GetNumLEDs() (int, error) { return NumLEDs, nil }

// SetLEDs records a painted frame; LEDs outside the bar are ignored.
func (s *Sink) SetLEDs(leds ...led.Led) error {
	s.mu.Lock()
	for _, l := range leds {
		if l.ID >= 0 && l.ID < NumLEDs {
			s.rgb[3*l.ID], s.rgb[3*l.ID+1], s.rgb[3*l.ID+2] = l.R, l.G, l.B
		}
	}
	changed := s.changed
	s.mu.Unlock()
	if changed != nil {
		changed()
	}
	return nil
}

// frame is the last frame as the link's 72 lowercase hex chars
// (12 × RRGGBB), all black before the first paint.
func (s *Sink) frame() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return hex.EncodeToString(s.rgb[:])
}
