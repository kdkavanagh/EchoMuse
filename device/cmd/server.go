//go:build server

// Command server is the EchoMuse device firmware, one binary for every
// supported image: it detects the platform (internal/platform), builds the
// physical server, the audio supervisor (capture, mixer, focus, wake
// detector, alert executor, uplink leases) and the three-socket controller
// link, and runs them until SIGTERM/SIGINT.
//
// What runs where:
//
//	                         Dot (fireos6)           Show, Fire OS (fireos6_crown)  Show, LineageOS (lineage18_crown)
//	capture / render         libmixerAPI (mixer)     OpenSL ES (AudioFlinger)       AAudio (AudioFlinger)
//	mic waits for NTP step   yes (timesync)          no: Android owns the clock     no
//	Wi-Fi join/recover/scan  yes (internal/wifi)     no: Android owns Wi-Fi         no
//	stop smarthomewifid      yes                     no                             no
//	BLE proxy scanner        yes (/dev/stpbt)        no: Android owns Bluetooth     no
//	LED ring, buttons, jack  yes                     no hardware / Android's keys   companion app's bar and touch zone; no jack
//	volume                   codec DAC (tinymix 61)  STREAM_MUSIC 0–30 (androidvol) STREAM_MUSIC 0–15 (androidvol)
//	privacy mute             yes                     deferred                       deferred
//	screen wake              no                      Fully Kiosk REST (kiosk)       companion app (internal/companion)
//	companion app            embedded, never run     embedded, never run            installed, kept running, linked on 127.0.0.1:8771
//	ambient light            yes                     yes                            yes, als_ps polling switched on
//	stats                    yes                     yes (no wpa_cli link stats)    yes (no wpa_cli link stats)
//	scratch (heap profiles)  /tmp                    /data/local/tmp/echomuse       /data/local/tmp/echomuse
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"syscall"
	"time"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/androidvol"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/bindings/als"
	internalbuttons "github.com/wilbowes/EchoMuse/internal/bindings/buttons"
	"github.com/wilbowes/EchoMuse/internal/bindings/jack"
	"github.com/wilbowes/EchoMuse/internal/bindings/slmic"
	"github.com/wilbowes/EchoMuse/internal/bindings/slspeaker"
	"github.com/wilbowes/EchoMuse/internal/bluetooth"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/companion"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/kiosk"
	"github.com/wilbowes/EchoMuse/internal/platform"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/render"
	"github.com/wilbowes/EchoMuse/internal/server"
	"github.com/wilbowes/EchoMuse/internal/supervisor"
	"github.com/wilbowes/EchoMuse/internal/timesync"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wifi"
	pkgbuttons "github.com/wilbowes/EchoMuse/pkg/buttons"
	pkgmic "github.com/wilbowes/EchoMuse/pkg/mic"
)

const (
	// legacyModelDir held the removed on-device openWakeWord/BCResNet
	// copies; v1 deletes it on start and never loads from it (§18.2).
	legacyModelDir = "/data/local/share/echomuse/oww"

	statsInterval = 30 * time.Second
	// micClockWait bounds how long the mixer microphone waits for the first
	// NTP sync. A cold boot synced 10–11 s after start on both Fire OS 6
	// Dots measured (2026-10-11); a Dot with no NTP egress opens it anyway,
	// and a late step is then survived by the recorder's re-open.
	micClockWait = 30 * time.Second
	memLogEvery  = 10 // stats ticks (~5 min) between [mem] log lines
	// wifiResultRetries × wifiResultInterval bounds at-least-once delivery
	// of a wifi_result (the dashboard gives up after 4 min).
	wifiResultRetries  = 30
	wifiResultInterval = 10 * time.Second
)

func main() {
	log.SetOutput(os.Stdout)
	if err := run(); err != nil {
		log.Fatal(err)
	}
}

func run() error {
	log.Printf("EchoMuse %s starting", client.Version)
	kind, err := platform.Detect()
	if err != nil {
		return err
	}
	dot := kind == platform.KindDot
	log.Printf("Platform: %s (%s, %q)", kind.Platform(), kind, platform.OSVersion())
	deviceID := client.GetSerialNo()
	log.Printf("Device ID: %s", deviceID)

	if dot {
		// A Wi-Fi change that never committed is rolled back before
		// anything uses the network.
		wifi.RecoverIfPending()
		// Nothing else joins Wi-Fi once wifisvc is stopped. It can wait
		// ~45 s for association, so it must not hold up audio and the ring.
		go wifi.EnsureUp()
		// Amazon's sntpd cannot sync with wifisvc stopped, so the firmware
		// sets the clock once a route exists (internal/timesync). The mixer
		// microphone opens only after that first step (openAudio).
		go timesync.Run()
		// Amazon's Wi-Fi Simple Setup daemon has no use here and was
		// observed busy-looping; stopping it is idempotent.
		_ = exec.Command("stop", "smarthomewifid").Run()
	}
	if kind == platform.KindCrownLineage {
		// Nothing on LineageOS keeps the light sensor polling; without
		// this its lux file is a stale cache (docs/echo-show-crown.md §8.5).
		als.EnablePolling()
	}
	// Every image is an MT8163 under MediaTek's hotplug; the Fire OS Show
	// already boots with this floor, so there it changes nothing.
	applyCoreFloor()
	if err := os.RemoveAll(legacyModelDir); err != nil {
		log.Printf("remove %s: %v", legacyModelDir, err)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()
	go dumpHeapOnSIGUSR1(kind.ScratchDir())

	bootID, err := alerts.ReadBootID()
	if err != nil {
		return err
	}
	cfg := config.New()
	phys, vol := newPhysical(kind)
	phys.SetLinkState(server.LinkDown)
	if dot {
		go func() {
			if err := phys.InitLEDs(); err != nil {
				log.Printf("LED ring unavailable: %v", err)
			}
		}()
	}

	mic, sink, err := openAudio(kind)
	if err != nil {
		return fmt.Errorf("audio: %w", err)
	}
	if dot {
		phys.SetHeadphones(jack.Inserted())
	}

	speech, err := assets.Open(wakeword.SpeechDir)
	if err != nil {
		return err
	}
	alertSounds, err := assets.Open(alerts.AssetDir(alerts.DefaultRoot))
	if err != nil {
		return err
	}

	var sup *supervisor.Supervisor
	var ble *bluetooth.Scanner // the Dot's BLE proxy; nil on the Show
	if dot {
		ble = bluetooth.NewScanner(func(batch []bluetooth.Advert) {
			sup.SendRetained(proto.TypeBLEAdverts, proto.BLEAdverts{Adverts: batch})
		})
	}
	// On LineageOS the companion app is the ring and the action button
	// (docs/echo-show-companion-plan.md): its bar shows every ring paint
	// and a touch in its zone presses the Dot's action button.
	var app *companion.Server
	var appInstaller *companion.Installer
	var appHealth *companion.Health
	if kind == platform.KindCrownLineage {
		appHealth = &companion.Health{}
		appInstaller = companion.NewInstaller(companion.InstallerConfig{Runner: companion.ExecRunner{}, Dir: kind.ScratchDir(), Health: appHealth})
		app = companion.NewServer(companion.ServerConfig{
			DeviceID: deviceID, Firmware: client.Version, Health: appHealth,
			Press:  func(ev pkgbuttons.ButtonClickEvent) { sup.DotButton(ev) },
			Linked: appInstaller.SetLinked, Mismatch: appInstaller.Reinstall, Overlay: appInstaller.Grant,
		})
		phys.AttachLEDs(app.Sink())
	}
	stats := statsCollector{wpaCLI: dot, companion: appHealth}
	sendStats := func() {
		st := stats.collect()
		if ble != nil {
			bs := ble.Stats()
			st.Ble = &bs
		}
		sup.SendRetained(proto.TypeStats, st)
	}
	supCfg := supervisor.Config{
		DeviceID:        deviceID,
		FirmwareVersion: client.Version,
		BootID:          bootID,
		Platform:        kind.Platform(),
		IP:              client.LocalIP,
		AmbientStatus: func() *als.Status {
			st := als.Report()
			return &st
		},
		AmbientReadable: als.Present,
		Mic:             mic,
		AFEMetadata:     true, // both capture backends decode micAsr / VOICE_RECOGNITION metadata into Block.AFE
		Physical:        phys,
		DeviceConfig:    cfg,
		SpeechStore:     speech,
		AlertStore:      alertSounds,
		Retained:        supervisor.RetainedHooks{Ready: func() { go sendStats() }},
	}
	if dot {
		supCfg.Retained = supervisor.RetainedHooks{
			ConfigApplied: func(v config.Values) { ble.SetEnabled(v.BLEProxyEnabled) },
			WifiChange:    func(ssid, psk string) { go changeWifi(sup, ssid, psk) },
			WifiCommit:    wifi.Commit,
			WifiScan:      func() { go scanWifi(sup) },
			Ready: func() {
				go sendStats()
				if r := wifi.PendingResult(); r != nil {
					sendWifiResult(sup, r)
				}
			},
		}
	} else if app != nil {
		// The app lights the screen; Fully's REST API is not used.
		supCfg.ScreenOn = func(string, string) { app.ScreenWake() }
		supCfg.Companion = app
	} else {
		supCfg.ScreenOn = kiosk.New().ScreenOn
	}
	sup, err = supervisor.Assemble(supCfg, supervisor.Deps{
		Sink:      sink,
		LoadModel: wakeword.LoadORT,
		Alerts:    alerts.Config{BootID: bootID},
	})
	if err != nil {
		mic.Close()
		_ = sink.Close()
		return err
	}

	if dot {
		ble.SetEnabled(cfg.Get().BLEProxyEnabled)
		buttons, err := internalbuttons.NewButtonController()
		if err != nil {
			return fmt.Errorf("buttons: %w", err)
		}
		buttons.SetVolumeCallback(sup.VolumeButton)
		buttons.SetMuteCallback(sup.MuteButton)
		if _, err := buttons.SubscribeToButton(sup.DotButton); err != nil {
			return fmt.Errorf("buttons: %w", err)
		}
		go jack.Watch(ctx, phys.SetHeadphones)
	} else if vol != nil {
		// The volume keys and the kiosk move Android's index; report
		// those changes as the Dot reports a button press.
		go vol.Watch(ctx, func(index int) { phys.ExternalVolumeChange(androidvol.LevelForIndex(index, vol.MaxIndex())) })
	}
	if app != nil {
		// Neither returns before ctx ends, and no failure of theirs stops
		// the firmware (plan decision 5).
		go appInstaller.Run(ctx)
		go app.Run(ctx)
	}

	go als.Watch(ctx, func(lux int) {
		sup.SendRetained(proto.TypeAmbientLight, proto.AmbientLight{Lux: lux})
	})
	go client.New(client.Config{DeviceID: deviceID}, sup).Run(ctx)
	go reportStats(ctx, sup, sendStats)

	log.Println("Ready")
	runErr := sup.Run(ctx)
	log.Printf("Shutting down")
	if ble != nil {
		ble.SetEnabled(false)
	}
	if err := phys.Close(); err != nil {
		log.Printf("amp off: %v", err)
	}
	return runErr
}

// newPhysical builds the physical server. The Dot drives its codec DAC,
// ring and privacy mute. On the Show volume is Android's STREAM_MUSIC index
// (internal/androidvol) behind the image's binder codes, which are verified
// here: when the check fails volume control is disabled with the reason and
// vol is nil. The Show has no privacy mute the firmware integrates yet.
func newPhysical(kind platform.Kind) (phys *server.Server, vol *androidvol.Control) {
	if kind == platform.KindDot {
		return server.New(server.Config{}), nil
	}
	codes := androidvol.FireOS
	if kind == platform.KindCrownLineage {
		codes = androidvol.Lineage18
	}
	vol = androidvol.New(codes)
	cfg := server.Config{Hardware: server.NewAndroidHardware(vol), NoPrivacyMute: true}
	if err := vol.Verify(); err != nil {
		cfg.VolumeUnavailable = "Android STREAM_MUSIC control failed its startup check: " + err.Error()
		vol = nil
	} else {
		cfg.VolumeAuthoritative = true
	}
	return server.New(cfg), vol
}

// openAudio opens the platform's capture and render streams: libmixerAPI on
// the Dot (docs/fireos6-port.md §3), OpenSL ES on the Fire OS Show
// (docs/echo-show-port-plan.md decision 3), AAudio on the LineageOS Show
// (docs/echo-show-lineage-plan.md decision 3).
func openAudio(kind platform.Kind) (pkgmic.Microphone, render.Sink, error) {
	var (
		mic     pkgmic.Microphone
		err     error
		newSink func() (render.Sink, error)
	)
	switch kind {
	case platform.KindCrown:
		mic, err = slmic.OpenOpenSL()
		newSink = func() (render.Sink, error) { return slspeaker.NewOpenSL() }
	case platform.KindCrownLineage:
		mic, err = slmic.OpenAAudio()
		newSink = func() (render.Sink, error) { return slspeaker.NewAAudio() }
	default:
		mic, err = slmic.OpenMixer(timesync.SettledWithin(micClockWait))
		newSink = func() (render.Sink, error) { return slspeaker.NewMixer() }
	}
	if err != nil {
		return nil, nil, err
	}
	sink, err := newSink()
	if err != nil {
		mic.Close()
		return nil, nil, err
	}
	return mic, sink, nil
}

// reportStats sends the retained stats body every 30 s and, every ~5 min,
// logs Go runtime memory accounting locally and to the controller.
func reportStats(ctx context.Context, sup *supervisor.Supervisor, sendStats func()) {
	ticker := time.NewTicker(statsInterval)
	defer ticker.Stop()
	for tick := 0; ; tick++ {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		sendStats()
		if tick%memLogEvery != 0 {
			continue
		}
		var ms runtime.MemStats
		runtime.ReadMemStats(&ms)
		line := fmt.Sprintf("[mem] goroutines=%d heap_alloc=%dKB heap_sys=%dKB heap_idle=%dKB released=%dKB stack=%dKB rss=%dKB num_gc=%d pause_total=%dms",
			runtime.NumGoroutine(), ms.HeapAlloc/1024, ms.HeapSys/1024, ms.HeapIdle/1024,
			ms.HeapReleased/1024, ms.StackSys/1024, selfRSSKb(), ms.NumGC, ms.PauseTotalNs/1e6)
		log.Print(line)
		sup.SendRetained(proto.TypeLog, proto.Log{Level: proto.LogInfo, Message: line})
	}
}

// changeWifi runs the safe network switch, then delivers its outcome with
// at-least-once semantics until the controller's wifi_commit clears it.
func changeWifi(sup *supervisor.Supervisor, ssid, psk string) {
	wifi.Change(ssid, psk, sup.Connected)
	for range wifiResultRetries {
		r := wifi.PendingResult()
		if r == nil {
			return
		}
		if sup.Connected() {
			sendWifiResult(sup, r)
		}
		time.Sleep(wifiResultInterval)
	}
}

func sendWifiResult(sup *supervisor.Supervisor, r *wifi.Result) {
	sup.SendRetained(proto.TypeWifiResult, proto.WifiResult{OK: r.OK, SSID: r.SSID, Error: r.Error})
}

func scanWifi(sup *supervisor.Supervisor) {
	nets, err := wifi.Scan()
	body := proto.WifiScanResult{Networks: nets}
	if err != nil {
		body = proto.WifiScanResult{Error: err.Error()}
	}
	sup.SendRetained(proto.TypeWifiScanRes, body)
}

// dumpHeapOnSIGUSR1 writes a heap profile to <dir>/heap-{0,1}.pprof
// (alternating) on SIGUSR1, dir being the platform's scratch directory; the
// device accepts no inbound connections, so profiles are pulled over the
// shell plane.
func dumpHeapOnSIGUSR1(dir string) {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGUSR1)
	for slot := 0; ; slot = 1 - slot {
		<-ch
		runtime.GC()
		if err := os.MkdirAll(dir, 0o755); err != nil {
			log.Printf("[pprof] %v", err)
			continue
		}
		path := filepath.Join(dir, fmt.Sprintf("heap-%d.pprof", slot))
		f, err := os.Create(path)
		if err != nil {
			log.Printf("[pprof] create %s: %v", path, err)
			continue
		}
		if err := pprof.WriteHeapProfile(f); err != nil {
			log.Printf("[pprof] write %s: %v", path, err)
		} else {
			log.Printf("[pprof] heap profile written to %s", path)
		}
		f.Close()
	}
}
