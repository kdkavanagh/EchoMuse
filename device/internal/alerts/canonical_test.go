package alerts

import "testing"

// Expected values were produced by CPython 3 json.dumps(obj, sort_keys=True,
// separators=(",", ":"), ensure_ascii=False) and uuid.uuid5, the controller's
// encoders.
func TestCanonicalJSONMatchesPython(t *testing.T) {
	in := `{ "z": 1, "a": "é\u2028\"\\\n\u0001",
	  "m": [1.5, 1e20, 1e-5, 0.1, -0.0, 100000000000000000000000, true, null],
	  "b": {"y": 2, "x": "日本"} }`
	want := `{"a":"é` + "\u2028" + `\"\\\n\u0001","b":{"x":"日本","y":2},` +
		`"m":[1.5,1e+20,1e-05,0.1,-0.0,100000000000000000000000,true,null],"z":1}`
	got, err := CanonicalJSON([]byte(in))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != want {
		t.Fatalf("canonical JSON\n got %s\nwant %s", got, want)
	}
	if _, err := CanonicalJSON([]byte(`{"a":1} {"b":2}`)); err == nil {
		t.Fatal("trailing value accepted")
	}
}

func TestPythonFloatRepr(t *testing.T) {
	for in, want := range map[string]string{
		"1.0": "1.0", "0.75": "0.75", "1e16": "1e+16", "123456789012345.6": "123456789012345.6",
		"0.0001": "0.0001", "0.00001234": "1.234e-05", "2.5e-300": "2.5e-300", "-3.0": "-3.0",
	} {
		got, err := CanonicalJSON([]byte(in))
		if err != nil || string(got) != want {
			t.Errorf("%s -> %s (%v), want %s", in, got, err, want)
		}
	}
}

func TestUUID5MatchesPython(t *testing.T) {
	parent := "0b8f4e0a-3c4b-5e6d-9f10-112233445566"
	if got := SnoozeChildScheduleID(parent); got != "9cc967de-5fa3-5373-b4f0-d9841f3f79f7" {
		t.Fatalf("child schedule %s", got)
	}
	occ, err := OccurrenceID("9cc967de-5fa3-5373-b4f0-d9841f3f79f7", "2026-09-23T06:39:01-05:00")
	if err != nil || occ != "164ed469-897e-5b59-b53b-23431a19381b" {
		t.Fatalf("child occurrence %s %v", occ, err)
	}
	if got := UUID5(NamespaceURL, "calendar.echomuse_office/abc").String(); got != "09e72c24-77e1-5473-859a-025b5a1115df" {
		t.Fatalf("ui schedule %s", got)
	}
	u := NewUUID4()
	if u[6]>>4 != 4 || u[8]>>6 != 2 {
		t.Fatalf("not a v4 uuid: %s", u)
	}
	if p, err := ParseUUID(u.String()); err != nil || p != u {
		t.Fatalf("round trip %s: %v", u, err)
	}
}
