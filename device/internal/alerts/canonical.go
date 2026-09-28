// Package alerts is the device alert cache, journal, and executor (SPEC §10,
// §16.3–§16.5): it stores controller-delivered alarm occurrences durably,
// rings them and HA timer rings through one local queue, and journals local
// dismiss/snooze/expire operations until the controller confirms them.
package alerts

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"math/big"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"
)

// decodeGeneric decodes one JSON value keeping numbers as json.Number so the
// canonical form reproduces the sender's integers and floats exactly.
func decodeGeneric(raw []byte) (any, error) {
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil, err
	}
	var extra any
	if err := dec.Decode(&extra); err == nil {
		return nil, fmt.Errorf("alerts: trailing data after JSON value")
	} else if err != io.EOF {
		return nil, err
	}
	return v, nil
}

// appendCanonical appends the canonical JSON of v (SPEC §16.4: sorted keys, no
// whitespace, UTF-8), byte-identical to Python's
// json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False).
// v is a value produced by decodeGeneric.
func appendCanonical(b []byte, v any) ([]byte, error) {
	switch x := v.(type) {
	case nil:
		return append(b, "null"...), nil
	case bool:
		if x {
			return append(b, "true"...), nil
		}
		return append(b, "false"...), nil
	case string:
		return appendPyString(b, x), nil
	case json.Number:
		return appendPyNumber(b, string(x))
	case []any:
		b = append(b, '[')
		for i, e := range x {
			if i > 0 {
				b = append(b, ',')
			}
			var err error
			if b, err = appendCanonical(b, e); err != nil {
				return nil, err
			}
		}
		return append(b, ']'), nil
	case map[string]any:
		keys := make([]string, 0, len(x))
		for k := range x {
			keys = append(keys, k)
		}
		// Byte order of UTF-8 equals code-point order, which is Python's str order.
		sort.Strings(keys)
		b = append(b, '{')
		for i, k := range keys {
			if i > 0 {
				b = append(b, ',')
			}
			b = appendPyString(b, k)
			b = append(b, ':')
			var err error
			if b, err = appendCanonical(b, x[k]); err != nil {
				return nil, err
			}
		}
		return append(b, '}'), nil
	default:
		return nil, fmt.Errorf("alerts: canonical JSON: unsupported type %T", v)
	}
}

// appendPyString escapes like Python's json encoder with ensure_ascii=False:
// only '"', '\\' and control characters below 0x20 are escaped.
func appendPyString(b []byte, s string) []byte {
	const hex = "0123456789abcdef"
	b = append(b, '"')
	for i := 0; i < len(s); {
		c := s[i]
		if c >= utf8.RuneSelf {
			r, size := utf8.DecodeRuneInString(s[i:])
			b = utf8.AppendRune(b, r)
			i += size
			continue
		}
		switch c {
		case '"':
			b = append(b, '\\', '"')
		case '\\':
			b = append(b, '\\', '\\')
		case '\n':
			b = append(b, '\\', 'n')
		case '\r':
			b = append(b, '\\', 'r')
		case '\t':
			b = append(b, '\\', 't')
		case '\b':
			b = append(b, '\\', 'b')
		case '\f':
			b = append(b, '\\', 'f')
		default:
			if c < 0x20 {
				b = append(b, '\\', 'u', '0', '0', hex[c>>4], hex[c&0xf])
			} else {
				b = append(b, c)
			}
		}
		i++
	}
	return append(b, '"')
}

// appendPyNumber re-emits a JSON number the way Python would after parsing it:
// integers as exact decimal, floats in repr() form.
func appendPyNumber(b []byte, s string) ([]byte, error) {
	if !strings.ContainsAny(s, ".eE") {
		n, ok := new(big.Int).SetString(s, 10)
		if !ok {
			return nil, fmt.Errorf("alerts: bad JSON integer %q", s)
		}
		return n.Append(b, 10), nil
	}
	f, err := strconv.ParseFloat(s, 64)
	if err != nil {
		return nil, fmt.Errorf("alerts: bad JSON number %q", s)
	}
	return appendPyFloat(b, f)
}

// appendPyFloat formats f as Python's float repr: shortest round-trip digits,
// positional notation for decimal exponents in [-4, 16), otherwise d.ddde±XX.
func appendPyFloat(b []byte, f float64) ([]byte, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return nil, fmt.Errorf("alerts: non-finite number")
	}
	if f == 0 {
		if math.Signbit(f) {
			return append(b, "-0.0"...), nil
		}
		return append(b, "0.0"...), nil
	}
	e := strconv.FormatFloat(f, 'e', -1, 64) // [-]d[.ddd]e±XX
	neg := e[0] == '-'
	if neg {
		e = e[1:]
		b = append(b, '-')
	}
	ePos := strings.IndexByte(e, 'e')
	digits := strings.Replace(e[:ePos], ".", "", 1)
	exp, err := strconv.Atoi(e[ePos+1:])
	if err != nil {
		return nil, err
	}
	if exp >= -4 && exp < 16 {
		if exp < 0 {
			b = append(b, "0."...)
			for i := 0; i < -exp-1; i++ {
				b = append(b, '0')
			}
			return append(b, digits...), nil
		}
		if len(digits) <= exp+1 {
			b = append(b, digits...)
			for i := len(digits); i < exp+1; i++ {
				b = append(b, '0')
			}
			return append(b, ".0"...), nil
		}
		b = append(b, digits[:exp+1]...)
		b = append(b, '.')
		return append(b, digits[exp+1:]...), nil
	}
	b = append(b, digits[0])
	if len(digits) > 1 {
		b = append(b, '.')
		b = append(b, digits[1:]...)
	}
	b = append(b, 'e')
	if exp < 0 {
		b = append(b, '-')
		exp = -exp
	} else {
		b = append(b, '+')
	}
	if exp < 10 {
		b = append(b, '0')
	}
	return strconv.AppendInt(b, int64(exp), 10), nil
}
