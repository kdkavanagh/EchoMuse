package server

import (
	"os"
	"path/filepath"
	"sync"
	"testing"
)

// fakeStream is Android's STREAM_MUSIC index; max 0 stands for Fire OS's 30.
type fakeStream struct {
	mu    sync.Mutex
	index int
	max   int
	sets  []int
}

func (f *fakeStream) MaxIndex() int {
	if f.max == 0 {
		return 30
	}
	return f.max
}

func (f *fakeStream) Index() (int, error) { f.mu.Lock(); defer f.mu.Unlock(); return f.index, nil }
func (f *fakeStream) SetIndex(i int) error {
	f.mu.Lock()
	f.index = i
	f.sets = append(f.sets, i)
	f.mu.Unlock()
	return nil
}
func (f *fakeStream) setCount() int { f.mu.Lock(); defer f.mu.Unlock(); return len(f.sets) }

// newShowServer builds the server as cmd/server.go does on the Show.
func newShowServer(t *testing.T, index int, statePath string) (*Server, *fakeStream, *[]int) {
	t.Helper()
	vol := &fakeStream{index: index}
	if statePath == "" {
		statePath = filepath.Join(t.TempDir(), "state.json")
	}
	s := New(Config{Hardware: NewAndroidHardware(vol), StatePath: statePath, NoPrivacyMute: true, VolumeAuthoritative: true})
	var reported []int
	s.SetVolumeChangeCallback(func(l int) { reported = append(reported, l) })
	return s, vol, &reported
}

// The wire level maps to the STREAM_MUSIC index proportionally over the
// build's range, and the Show's codec, amp and LED calls touch nothing.
func TestAndroidHardwareMapsLevelToIndex(t *testing.T) {
	vol := &fakeStream{index: 12}
	hw := NewAndroidHardware(vol)
	if l, err := hw.ReadDAC(); err != nil || l != 51 {
		t.Fatalf("ReadDAC = %d, %v; want 51", l, err)
	}
	for level, index := range map[int]int{85: 20, 127: 30, 0: 0, 200: 30, -3: 0} {
		if err := hw.SetDAC(level); err != nil || vol.index != index {
			t.Errorf("SetDAC(%d) set index %d (%v), want %d", level, vol.index, err, index)
		}
	}
	lineage := &fakeStream{index: 5, max: 15}
	lhw := NewAndroidHardware(lineage)
	if l, err := lhw.ReadDAC(); err != nil || l != 42 {
		t.Fatalf("LineageOS ReadDAC = %d, %v; want 42", l, err)
	}
	for level, index := range map[int]int{85: 10, 127: 15, 0: 0, 200: 15} {
		if err := lhw.SetDAC(level); err != nil || lineage.index != index {
			t.Errorf("LineageOS SetDAC(%d) set index %d (%v), want %d", level, lineage.index, err, index)
		}
	}
	n := vol.setCount()
	if hw.SetADCMute(true) != nil || hw.SetSpeakerAmp(false) != nil || hw.SetMuteLED(true) != nil || vol.setCount() != n {
		t.Error("mute, amp or LED call did something on the Show")
	}
}

// Android's index is the authority: volume starts seeded from it, so the
// controller's startupVolume never overrides it, and a key change is
// reported without being written back.
func TestShowVolumeIsAndroidsIndex(t *testing.T) {
	s, vol, reported := newShowServer(t, 12, "")
	if l, seeded := s.VolumeState(); l != 51 || !seeded {
		t.Fatalf("start: level %d seeded %v, want 51 seeded", l, seeded)
	}
	s.SeedVolume(100)
	if vol.setCount() != 0 || len(*reported) != 0 {
		t.Fatalf("startupVolume overrode Android's index: sets %v reported %v", vol.sets, *reported)
	}
	s.ExternalVolumeChange(55) // index 13 from a key press
	if l, _ := s.VolumeState(); l != 55 || vol.setCount() != 0 || len(*reported) != 1 || (*reported)[0] != 55 {
		t.Fatalf("key change: level %d sets %v reported %v", l, vol.sets, *reported)
	}
	s.SetVolume(85) // dashboard
	if vol.index != 20 || len(*reported) != 2 || (*reported)[1] != 85 {
		t.Fatalf("volume_set: index %d reported %v", vol.index, *reported)
	}
}

// A foreground alert sets its own index and restores media volume after; a
// key change meanwhile moves only the occurrence, as a button would.
func TestShowKeyChangeDuringAlertMovesOnlyTheOccurrence(t *testing.T) {
	s, vol, reported := newShowServer(t, 12, "")
	full := 1.0
	s.SetAlertAudio(true, "a1", true, &full)
	if vol.index != 30 {
		t.Fatalf("alert index %d, want 30", vol.index)
	}
	s.ExternalVolumeChange(110)
	if l, _ := s.VolumeState(); l != 51 || len(*reported) != 0 {
		t.Fatalf("key change during alert: media %d reported %v", l, *reported)
	}
	s.SetAlertAudio(false, "a1", false, nil)
	if vol.index != 12 {
		t.Fatalf("restored index %d, want 12", vol.index)
	}
}

// On a Show with a ring display attached (the LineageOS companion bar),
// Android's own volume panel is the volume UI: a key or dashboard change
// never paints the arc over the ring (docs/echo-show-companion-plan.md
// decision 12). The arc follows only the Dot's volume buttons.
func TestShowVolumeChangeNeverPaintsTheArc(t *testing.T) {
	s, _, _ := newShowServer(t, 12, "")
	leds := &fakeLEDs{}
	s.AttachLEDs(leds)
	s.SetLinkState(LinkUp)
	black := blackFrame()
	s.ExternalVolumeChange(110)
	s.SetVolume(90)
	for i, l := range leds.frame() {
		if l != black[i] {
			t.Fatalf("volume change painted %v", leds.frame())
		}
	}
}

// When the mechanism failed its startup check, nothing is applied or reported.
// session.hello carries a null level with the reason.
func TestShowVolumeUnavailable(t *testing.T) {
	vol := &fakeStream{index: 12}
	s := New(Config{Hardware: NewAndroidHardware(vol), StatePath: filepath.Join(t.TempDir(), "state.json"),
		NoPrivacyMute: true, VolumeUnavailable: "getStreamMaxVolume answered 15"})
	var reported []int
	s.SetVolumeChangeCallback(func(l int) { reported = append(reported, l) })
	s.SeedVolume(100)
	s.SetVolume(60)
	s.VolumeStepUp()
	s.ExternalVolumeChange(70)
	full := 1.0
	s.SetAlertAudio(true, "a1", true, &full)
	s.SetAlertAudio(false, "a1", false, nil)
	if _, seeded := s.VolumeState(); seeded || vol.setCount() != 0 || len(reported) != 0 {
		t.Fatalf("disabled volume: seeded %v sets %v reported %v", seeded, vol.sets, reported)
	}
	if s.VolumeUnavailable() == "" {
		t.Fatal("no reason for the disabled volume")
	}
	if ok, _, _ := newShowServer(t, 12, ""); ok.VolumeUnavailable() != "" {
		t.Fatalf("working volume reported unavailable: %q", ok.VolumeUnavailable())
	}
}

// The Show integrates no privacy mute yet (Phase 7): a persisted mute is
// not restored and a toggle does nothing.
func TestShowHasNoPrivacyMute(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	if err := os.WriteFile(path, []byte(`{"muted":true}`), 0o644); err != nil {
		t.Fatal(err)
	}
	s, _, _ := newShowServer(t, 12, path)
	var changes []bool
	s.SetMuteChangeCallback(func(m bool) { changes = append(changes, m) })
	s.MuteToggle()
	if s.IsMuted() || len(changes) != 0 {
		t.Fatalf("muted %v changes %v", s.IsMuted(), changes)
	}
}
