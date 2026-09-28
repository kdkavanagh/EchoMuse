package server

import (
	"log"
	"sync"
)

// muteController owns the device-sovereign privacy state, physical ADC mute,
// red ring and discrete mute-button LED (SPEC §3.2, §11.2).
type muteController struct {
	mu sync.Mutex

	hw       Hardware
	ring     *ring
	path     string
	muted    bool
	onChange func(bool)
}

func newMuteController(hw Hardware, r *ring, path string) *muteController {
	if hw == nil {
		hw = systemHardware{}
	}
	m := &muteController{hw: hw, ring: r, path: path}
	if st, ok := loadDeviceState(path); ok {
		m.muted = st.Muted
	}
	m.apply(m.muted)
	return m
}

func (m *muteController) setCallback(cb func(bool)) {
	m.mu.Lock()
	m.onChange = cb
	m.mu.Unlock()
}

func (m *muteController) isMuted() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.muted
}

func (m *muteController) toggle() { m.set(!m.isMuted()) }

func (m *muteController) set(muted bool) {
	m.mu.Lock()
	if muted == m.muted {
		m.mu.Unlock()
		return
	}
	m.muted = muted
	cb := m.onChange
	m.mu.Unlock()
	m.apply(muted)
	if m.path != "" {
		saveDeviceState(m.path, deviceState{Muted: muted})
	}
	if cb != nil {
		cb(muted)
	}
}

func (m *muteController) apply(muted bool) {
	if err := m.hw.SetADCMute(muted); err != nil {
		log.Printf("[privacy] ADC mute=%v: %v", muted, err)
	}
	if err := m.hw.SetMuteLED(muted); err != nil {
		log.Printf("[privacy] button LED=%v: %v", muted, err)
	}
	m.ring.setMuted(muted)
}
