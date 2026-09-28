package server

import (
	"path/filepath"
	"sync"
	"testing"

	"github.com/wilbowes/EchoMuse/pkg/led"
)

type fakeHardware struct {
	mu    sync.Mutex
	dac   int
	sets  []int
	adc   bool
	amp   *bool
	muteL bool
}

func (f *fakeHardware) ReadDAC() (int, error) { f.mu.Lock(); defer f.mu.Unlock(); return f.dac, nil }
func (f *fakeHardware) SetDAC(l int) error {
	f.mu.Lock()
	f.dac = l
	f.sets = append(f.sets, l)
	f.mu.Unlock()
	return nil
}
func (f *fakeHardware) SetADCMute(m bool) error { f.mu.Lock(); f.adc = m; f.mu.Unlock(); return nil }
func (f *fakeHardware) SetSpeakerAmp(on bool) error {
	f.mu.Lock()
	f.amp = &on
	f.mu.Unlock()
	return nil
}
func (f *fakeHardware) SetMuteLED(on bool) error {
	f.mu.Lock()
	f.muteL = on
	f.mu.Unlock()
	return nil
}

func (f *fakeHardware) DAC() int { f.mu.Lock(); defer f.mu.Unlock(); return f.dac }
func (f *fakeHardware) Amp() *bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.amp
}

type fakeLEDs struct {
	mu   sync.Mutex
	last []led.Led
}

func (f *fakeLEDs) Init() error              { return nil }
func (f *fakeLEDs) GetNumLEDs() (int, error) { return numLEDs, nil }
func (f *fakeLEDs) SetLEDs(v ...led.Led) error {
	f.mu.Lock()
	f.last = append([]led.Led(nil), v...)
	f.mu.Unlock()
	return nil
}
func (f *fakeLEDs) frame() []led.Led { f.mu.Lock(); defer f.mu.Unlock(); return f.last }

func newTestServer(t *testing.T, dac int) (*Server, *fakeHardware) {
	t.Helper()
	hw := &fakeHardware{dac: dac}
	return New(Config{Hardware: hw, StatePath: filepath.Join(t.TempDir(), "state.json")}), hw
}

// §16.5: the DAC holds media volume, switches to the occurrence's volume for
// a foreground alert and returns to media volume on release.
func TestAlertForegroundOwnsDACAndRestoresMediaVolume(t *testing.T) {
	s, hw := newTestServer(t, 90)
	half := 0.5
	s.SetAlertAudio(true, "occ", true, &half)
	if got := hw.DAC(); got != 64 {
		t.Fatalf("foreground alert DAC %d, want 64 (0.5 × 127)", got)
	}
	s.SetAlertAudio(true, "occ", false, &half)
	if got := hw.DAC(); got != 90 {
		t.Fatalf("backgrounded alert DAC %d, want media 90", got)
	}
	s.SetAlertAudio(true, "occ", true, &half)
	s.SetAlertAudio(false, "", false, nil)
	if got := hw.DAC(); got != 90 {
		t.Fatalf("released alert DAC %d, want media 90", got)
	}
}

// A null occurrence volume rings at the current media volume.
func TestAlertWithoutVolumeUsesMediaVolume(t *testing.T) {
	s, hw := newTestServer(t, 100)
	s.SetAlertAudio(true, "occ", true, nil)
	if got := hw.DAC(); got != 100 {
		t.Fatalf("DAC %d, want media 100", got)
	}
}

// §16.5: volume buttons during a foreground alert adjust only that
// occurrence and are not reported or persisted as media volume.
func TestVolumeButtonsDuringForegroundAlertAdjustOnlyTheOccurrence(t *testing.T) {
	s, hw := newTestServer(t, 90)
	var reported []int
	s.SetVolumeChangeCallback(func(l int) { reported = append(reported, l) })
	half := 0.5
	s.SetAlertAudio(true, "occ", true, &half)
	s.VolumeStepUp()
	if got := hw.DAC(); got != 64+volumeStep {
		t.Fatalf("alert DAC after step %d, want %d", got, 64+volumeStep)
	}
	if level, _ := s.VolumeState(); level != 90 {
		t.Fatalf("media volume changed to %d", level)
	}
	if len(reported) != 0 {
		t.Fatalf("occurrence step reported as volume_state %v", reported)
	}
	s.SetAlertAudio(false, "", false, nil)
	if got := hw.DAC(); got != 90 {
		t.Fatalf("DAC after release %d, want 90", got)
	}
	s.VolumeStepUp()
	if level, seeded := s.VolumeState(); level != 90+volumeStep || !seeded {
		t.Fatalf("media step: level %d seeded %v", level, seeded)
	}
	if len(reported) != 1 || reported[0] != 90+volumeStep {
		t.Fatalf("media step reports %v", reported)
	}
}

// A remote volume change during a foreground alert updates media volume
// without taking the DAC from the alert.
func TestRemoteVolumeDuringAlertAppliesAfterRelease(t *testing.T) {
	s, hw := newTestServer(t, 90)
	half := 0.5
	s.SetAlertAudio(true, "occ", true, &half)
	s.SetVolume(30)
	if got := hw.DAC(); got != 64 {
		t.Fatalf("DAC %d during alert, want 64", got)
	}
	s.SetAlertAudio(false, "", false, nil)
	if got := hw.DAC(); got != 30 {
		t.Fatalf("DAC %d after release, want 30", got)
	}
}

// §16.5: with headphones inserted, a foreground alert re-enables the
// internal amp and restores the insertion route afterwards.
func TestForegroundAlertForcesSpeakerAmpOverHeadphones(t *testing.T) {
	s, hw := newTestServer(t, 90)
	s.SetHeadphones(true)
	if a := hw.Amp(); a == nil || *a {
		t.Fatalf("amp after headphone insert %v, want off", a)
	}
	s.SetAlertAudio(true, "occ", true, nil)
	if a := hw.Amp(); a == nil || !*a {
		t.Fatal("foreground alert did not force the amp on")
	}
	s.SetAlertAudio(true, "occ", false, nil)
	if a := hw.Amp(); *a {
		t.Fatal("backgrounded alert left the amp forced on")
	}
}

// startupVolume seeds once; a local change first makes the live value win.
func TestSeedVolumeHonoursOnlyTheFirstAuthority(t *testing.T) {
	s, hw := newTestServer(t, 90)
	s.VolumeStepDown()
	s.SeedVolume(20)
	if got := hw.DAC(); got != 90-volumeStep {
		t.Fatalf("seed overrode a local change: DAC %d", got)
	}
	s2, hw2 := newTestServer(t, 90)
	s2.SeedVolume(20)
	s2.SeedVolume(40)
	if got := hw2.DAC(); got != 20 {
		t.Fatalf("second seed applied: DAC %d", got)
	}
}

func TestStepsStayInsideTheButtonBand(t *testing.T) {
	cases := []struct{ in, want int }{
		{volumeButtonFloor - 40, volumeButtonFloor},
		{volumeButtonFloor - 1, volumeButtonFloor},
		{volumeButtonFloor + volumeStep, volumeButtonFloor + volumeStep},
		{volumeMax + 30, volumeMax},
	}
	for _, tc := range cases {
		if got := clampToButtonBand(tc.in); got != tc.want {
			t.Errorf("clampToButtonBand(%d) = %d, want %d", tc.in, got, tc.want)
		}
	}
}

// Privacy and alert indication are local layers above controller frames
// and survive controller loss (§11.2).
func TestRingLayerPrecedence(t *testing.T) {
	s, _ := newTestServer(t, 90)
	leds := &fakeLEDs{}
	s.ring.SetController(leds)

	green := solidFrame(0, 200, 0)
	s.SetLEDs(green)
	if leds.frame()[0].G != 200 {
		t.Fatal("controller frame not painted")
	}
	s.SetAlertIndication(true, false)
	if f := leds.frame()[0]; f.G == 200 || f.B == 0 {
		t.Fatalf("alert indication did not outrank controller layer: %+v", f)
	}
	s.MuteToggle()
	if f := leds.frame()[0]; f.R != 180 || f.G != 0 {
		t.Fatalf("mute ring not sovereign: %+v", f)
	}
	s.ClearControllerLEDs()
	s.MuteToggle()
	if f := leds.frame()[0]; f.B == 0 {
		t.Fatalf("alert indication lost after unmute: %+v", f)
	}
	s.SetAlertIndication(false, false)
	if f := leds.frame()[0]; f != (led.Led{ID: 0}) {
		t.Fatalf("cleared controller layer repainted: %+v", f)
	}
}

// Privacy state persists across restarts (device-sovereign).
func TestMuteRestoresFromPersistedState(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	hw := &fakeHardware{}
	s := New(Config{Hardware: hw, StatePath: path})
	s.MuteToggle()
	hw2 := &fakeHardware{}
	s2 := New(Config{Hardware: hw2, StatePath: path})
	if !s2.IsMuted() || !hw2.adc || !hw2.muteL {
		t.Fatalf("restart: muted %v adc %v led %v", s2.IsMuted(), hw2.adc, hw2.muteL)
	}
}
