package server

import (
	"fmt"
	"os/exec"
	"strings"

	internalled "github.com/wilbowes/EchoMuse/internal/bindings/led"
)

// Hardware is the server's physical codec/amp/GPIO boundary. Tests inject a
// fake; the system implementation uses the observed Dot controls (§16.5).
type Hardware interface {
	ReadDAC() (int, error)
	SetDAC(level int) error      // tinymix control 61, stereo
	SetADCMute(muted bool) error // all four codec ADC pairs
	SetSpeakerAmp(on bool) error // tinymix control 5
	SetMuteLED(on bool) error    // discrete gpio444 LED
}

type systemHardware struct{}

func (systemHardware) ReadDAC() (int, error) {
	out, err := exec.Command("tinymix", "-D", "0", "61").Output()
	if err != nil {
		return 0, err
	}
	var l, r int
	if _, err := fmt.Sscanf(string(out), "PCM Playback Volume: %d %d", &l, &r); err != nil {
		return 0, fmt.Errorf("parse %q: %w", strings.TrimSpace(string(out)), err)
	}
	return clamp(l), nil
}

func (systemHardware) SetDAC(level int) error {
	s := fmt.Sprintf("%d", clamp(level))
	out, err := exec.Command("tinymix", "-D", "0", "61", s, s).CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

var adcMuteControls = [...]string{
	"105", "106", // ADC_A
	"123", "124", // ADC_B
	"141", "142", // ADC_C
	"159", "160", // ADC_D
}

func (systemHardware) SetADCMute(muted bool) error {
	value := "0"
	if muted {
		value = "1"
	}
	var first error
	for _, ctl := range adcMuteControls {
		if out, err := exec.Command("tinymix", "-D", "0", ctl, value).CombinedOutput(); err != nil && first == nil {
			first = fmt.Errorf("ctl %s: %w: %s", ctl, err, strings.TrimSpace(string(out)))
		}
	}
	return first
}

func (systemHardware) SetSpeakerAmp(on bool) error {
	value := "Off"
	if on {
		value = "On"
	}
	out, err := exec.Command("tinymix", "-D", "0", "5", value).CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

func (systemHardware) SetMuteLED(on bool) error { return internalled.SetMuteButtonLED(on) }
