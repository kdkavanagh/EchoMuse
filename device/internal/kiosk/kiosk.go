// Package kiosk pokes the Show's kiosk browser. EchoMuse does not drive the
// display: the user's Fully Kiosk Browser owns it, with its own screen
// timeout and motion wake. The one thing the firmware asks of it is to light
// the screen when a wake word opens a candidate (screen_wake, config
// screenWake), through Fully's local REST API.
package kiosk

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	// requestTimeout bounds one screenOn request; the wake path never
	// waits for it.
	requestTimeout = 2 * time.Second
	// logEvery rate-limits failure logs: a kiosk that is not installed or
	// has its REST API off fails on every wake word.
	logEvery = time.Minute
	// maxBody bounds how much of a reply is read.
	maxBody = 4 << 10
)

// ScreenOnURL is Fully Kiosk's screenOn request against base, the REST
// API's root (config screenWakeUrl).
func ScreenOnURL(base, password string) string {
	return strings.TrimRight(base, "/") + "/?cmd=screenOn&type=json&password=" + url.QueryEscape(password)
}

// Waker sends screenOn requests, one at a time.
type Waker struct {
	client   *http.Client
	inFlight atomic.Bool

	mu         sync.Mutex
	lastLog    time.Time
	suppressed int
}

// New returns a Waker with the 2 s request timeout.
func New() *Waker { return &Waker{client: &http.Client{Timeout: requestTimeout}} }

// ScreenOn asks the kiosk to light the screen and returns at once. A request
// still in flight absorbs this one: the screen is already being lit.
func (w *Waker) ScreenOn(base, password string) {
	if !w.inFlight.CompareAndSwap(false, true) {
		return
	}
	go func() {
		defer w.inFlight.Store(false)
		if err := w.screenOn(ScreenOnURL(base, password)); err != nil {
			w.logFailure(err)
		}
	}()
}

// reply is the part of Fully's type=json answer that tells success apart:
// {"status":"OK",...}, or {"status":"Error","statustext":"..."} with HTTP
// 200 (a wrong password, for one).
type reply struct {
	Status     string `json:"status"`
	StatusText string `json:"statustext"`
}

func (w *Waker) screenOn(u string) error {
	resp, err := w.client.Get(u)
	if err != nil {
		// url.Error's text carries the URL, and with it the password.
		var uerr *url.Error
		if errors.As(err, &uerr) {
			err = uerr.Err
		}
		return fmt.Errorf("screenOn: %v", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxBody))
	if err != nil {
		return fmt.Errorf("screenOn: read reply: %v", err)
	}
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("screenOn: HTTP %d", resp.StatusCode)
	}
	var r reply
	if err := json.Unmarshal(body, &r); err == nil && r.Status != "" && !strings.EqualFold(r.Status, "OK") {
		return fmt.Errorf("screenOn: %s: %s", r.Status, r.StatusText)
	}
	return nil
}

func (w *Waker) logFailure(err error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if !w.lastLog.IsZero() && time.Since(w.lastLog) < logEvery {
		w.suppressed++
		return
	}
	if w.suppressed > 0 {
		log.Printf("[kiosk] %v (%d earlier failures not logged)", err, w.suppressed)
	} else {
		log.Printf("[kiosk] %v", err)
	}
	w.lastLog, w.suppressed = time.Now(), 0
}
