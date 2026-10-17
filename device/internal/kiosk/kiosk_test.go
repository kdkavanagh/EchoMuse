package kiosk

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// The request is {url}/?cmd=screenOn&type=json&password={urlencoded}.
func TestScreenOnURL(t *testing.T) {
	for _, tc := range []struct{ base, password, want string }{
		{"http://127.0.0.1:2323", "", "http://127.0.0.1:2323/?cmd=screenOn&type=json&password="},
		{"http://127.0.0.1:2323/", "secret", "http://127.0.0.1:2323/?cmd=screenOn&type=json&password=secret"},
		{"http://10.0.0.5:2323", "p&w=1 x/?#", "http://10.0.0.5:2323/?cmd=screenOn&type=json&password=p%26w%3D1+x%2F%3F%23"},
	} {
		if got := ScreenOnURL(tc.base, tc.password); got != tc.want {
			t.Errorf("ScreenOnURL(%q, %q) = %q, want %q", tc.base, tc.password, got, tc.want)
		}
	}
}

// Fully answers HTTP 200 either way; its JSON status tells success from a
// refusal such as a wrong password.
func TestScreenOnReplies(t *testing.T) {
	var gotQuery string
	reply := `{"status":"OK","statustext":"Switching the screen on"}`
	code := http.StatusOK
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotQuery = r.URL.RawQuery
		w.WriteHeader(code)
		_, _ = w.Write([]byte(reply))
	}))
	defer srv.Close()
	w := New()

	if err := w.screenOn(ScreenOnURL(srv.URL, "pw")); err != nil {
		t.Fatalf("ok reply: %v", err)
	}
	if gotQuery != "cmd=screenOn&type=json&password=pw" {
		t.Errorf("query %q", gotQuery)
	}
	reply = `{"status":"Error","statustext":"Please enter the correct password"}`
	if err := w.screenOn(ScreenOnURL(srv.URL, "bad")); err == nil || !strings.Contains(err.Error(), "password") {
		t.Errorf("refusal: %v", err)
	}
	reply, code = "", http.StatusInternalServerError
	if err := w.screenOn(ScreenOnURL(srv.URL, "pw")); err == nil {
		t.Error("HTTP 500 accepted")
	}
}

// A kiosk that is not listening fails without the password in the error.
func TestScreenOnErrorHidesPassword(t *testing.T) {
	srv := httptest.NewServer(http.NotFoundHandler())
	base := srv.URL
	srv.Close()
	err := New().screenOn(ScreenOnURL(base, "hunter2"))
	if err == nil || strings.Contains(err.Error(), "hunter2") {
		t.Fatalf("error %v", err)
	}
}

// ScreenOn never blocks the wake path, and a request in flight absorbs the
// next one.
func TestScreenOnIsAsyncAndSingleFlight(t *testing.T) {
	release := make(chan struct{})
	hits := make(chan struct{}, 4)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits <- struct{}{}
		<-release
		_, _ = w.Write([]byte(`{"status":"OK"}`))
	}))
	defer srv.Close()
	defer close(release)
	w := New()

	start := time.Now()
	w.ScreenOn(srv.URL, "")
	w.ScreenOn(srv.URL, "")
	if d := time.Since(start); d > 100*time.Millisecond {
		t.Fatalf("ScreenOn blocked for %v", d)
	}
	select {
	case <-hits:
	case <-time.After(2 * time.Second):
		t.Fatal("no request reached the kiosk")
	}
	select {
	case <-hits:
		t.Fatal("a second request went out while the first was in flight")
	case <-time.After(100 * time.Millisecond):
	}
}
