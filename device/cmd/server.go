//go:build server

// Command server is the EchoMuse Dot firmware: it builds the physical
// server, the audio supervisor (capture, mixer, focus, wake detector, alert
// executor, uplink leases) and the three-socket controller link, and runs
// them until SIGTERM/SIGINT.
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"runtime/pprof"
	"syscall"
	"time"

	"github.com/wilbowes/EchoMuse/internal/alerts"
	"github.com/wilbowes/EchoMuse/internal/assets"
	"github.com/wilbowes/EchoMuse/internal/bindings/als"
	internalbuttons "github.com/wilbowes/EchoMuse/internal/bindings/buttons"
	"github.com/wilbowes/EchoMuse/internal/bindings/jack"
	"github.com/wilbowes/EchoMuse/internal/bindings/slmic"
	"github.com/wilbowes/EchoMuse/internal/bindings/slspeaker"
	"github.com/wilbowes/EchoMuse/internal/bluetooth"
	"github.com/wilbowes/EchoMuse/internal/client"
	"github.com/wilbowes/EchoMuse/internal/config"
	"github.com/wilbowes/EchoMuse/internal/proto"
	"github.com/wilbowes/EchoMuse/internal/server"
	"github.com/wilbowes/EchoMuse/internal/supervisor"
	"github.com/wilbowes/EchoMuse/internal/wakeword"
	"github.com/wilbowes/EchoMuse/internal/wifi"
)

const (
	// legacyModelDir held the removed on-device openWakeWord/BCResNet
	// copies; v1 deletes it on start and never loads from it (§18.2).
	legacyModelDir = "/data/local/share/echomuse/oww"

	statsInterval = 30 * time.Second
	memLogEvery   = 10 // stats ticks (~5 min) between [mem] log lines
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
	deviceID := client.GetSerialNo()
	log.Printf("Device ID: %s", deviceID)

	// A Wi-Fi change that never committed is rolled back before anything
	// uses the network.
	wifi.RecoverIfPending()
	// Amazon's Wi-Fi Simple Setup daemon has no use here and was observed
	// busy-looping; stopping it is idempotent.
	_ = exec.Command("stop", "smarthomewifid").Run()
	applyCoreFloor()
	if err := os.RemoveAll(legacyModelDir); err != nil {
		log.Printf("remove %s: %v", legacyModelDir, err)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()
	go dumpHeapOnSIGUSR1()

	bootID, err := alerts.ReadBootID()
	if err != nil {
		return err
	}
	cfg := config.New()
	phys := server.New(server.Config{})
	phys.SetLinkState(server.LinkDown)
	go func() {
		if err := phys.InitLEDs(); err != nil {
			log.Printf("LED ring unavailable: %v", err)
		}
	}()

	mic, err := slmic.Open()
	if err != nil {
		return fmt.Errorf("capture: %w", err)
	}
	sink, err := slspeaker.New()
	if err != nil {
		mic.Close()
		return fmt.Errorf("render: %w", err)
	}
	phys.SetHeadphones(jack.Inserted())

	speech, err := assets.Open(wakeword.SpeechDir)
	if err != nil {
		return err
	}
	alertSounds, err := assets.Open(alerts.AssetDir(alerts.DefaultRoot))
	if err != nil {
		return err
	}

	var sup *supervisor.Supervisor
	ble := bluetooth.NewScanner(func(batch []bluetooth.Advert) {
		sup.SendRetained(proto.TypeBLEAdverts, proto.BLEAdverts{Adverts: batch})
	})
	var stats statsCollector
	sendStats := func() {
		st := stats.collect()
		bs := ble.Stats()
		st.Ble = &bs
		sup.SendRetained(proto.TypeStats, st)
	}
	sup, err = supervisor.Assemble(supervisor.Config{
		DeviceID:        deviceID,
		FirmwareVersion: client.Version,
		BootID:          bootID,
		IP:              client.LocalIP,
		AmbientStatus: func() *als.Status {
			st := als.Report()
			return &st
		},
		AmbientReadable: als.Present,
		Mic:             mic,
		Physical:        phys,
		DeviceConfig:    cfg,
		SpeechStore:     speech,
		AlertStore:      alertSounds,
		Retained: supervisor.RetainedHooks{
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
		},
	}, supervisor.Deps{
		Sink:      sink,
		LoadModel: wakeword.LoadORT,
		Alerts:    alerts.Config{BootID: bootID},
	})
	if err != nil {
		mic.Close()
		_ = sink.Close()
		return err
	}
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

	go als.Watch(ctx, func(lux int) {
		sup.SendRetained(proto.TypeAmbientLight, proto.AmbientLight{Lux: lux})
	})
	go jack.Watch(ctx, phys.SetHeadphones)
	go client.New(client.Config{DeviceID: deviceID}, sup).Run(ctx)
	go reportStats(ctx, sup, sendStats)

	log.Println("Ready")
	runErr := sup.Run(ctx)
	log.Printf("Shutting down")
	ble.SetEnabled(false)
	if err := phys.Close(); err != nil {
		log.Printf("amp off: %v", err)
	}
	return runErr
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

// dumpHeapOnSIGUSR1 writes a heap profile to /tmp/heap-{0,1}.pprof
// (alternating) on SIGUSR1; the device accepts no inbound connections, so
// profiles are pulled over the shell plane.
func dumpHeapOnSIGUSR1() {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGUSR1)
	for slot := 0; ; slot = 1 - slot {
		<-ch
		runtime.GC()
		path := fmt.Sprintf("/tmp/heap-%d.pprof", slot)
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
