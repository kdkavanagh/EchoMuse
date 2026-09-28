package alerts

import (
	"crypto/rand"
	"crypto/sha1"
	"encoding/hex"
	"fmt"
)

// UUID is an RFC 4122 UUID.
type UUID [16]byte

// NamespaceURL is RFC 4122's NAMESPACE_URL (6ba7b811-9dad-11d1-80b4-00c04fd430c8).
var NamespaceURL = UUID{0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8}

// ParseUUID accepts the hyphenated 36-character form, either case.
func ParseUUID(s string) (UUID, error) {
	var u UUID
	if len(s) != 36 || s[8] != '-' || s[13] != '-' || s[18] != '-' || s[23] != '-' {
		return u, fmt.Errorf("alerts: malformed uuid %q", s)
	}
	h := s[0:8] + s[9:13] + s[14:18] + s[19:23] + s[24:36]
	if _, err := hex.Decode(u[:], []byte(h)); err != nil {
		return u, fmt.Errorf("alerts: malformed uuid %q", s)
	}
	return u, nil
}

// String is the lowercase hyphenated form, as Python's str(uuid).
func (u UUID) String() string {
	var b [36]byte
	hex.Encode(b[0:8], u[0:4])
	b[8] = '-'
	hex.Encode(b[9:13], u[4:6])
	b[13] = '-'
	hex.Encode(b[14:18], u[6:8])
	b[18] = '-'
	hex.Encode(b[19:23], u[8:10])
	b[23] = '-'
	hex.Encode(b[24:36], u[10:16])
	return string(b[:])
}

// UUID5 is RFC 4122 version 5 (SHA-1) of name in namespace ns, equal to
// Python's uuid.uuid5(ns, name) for a str name.
func UUID5(ns UUID, name string) UUID {
	h := sha1.New()
	h.Write(ns[:])
	h.Write([]byte(name))
	var u UUID
	copy(u[:], h.Sum(nil))
	u[6] = u[6]&0x0f | 0x50
	u[8] = u[8]&0x3f | 0x80
	return u
}

// NewUUID4 returns a random version 4 UUID.
func NewUUID4() UUID {
	var u UUID
	if _, err := rand.Read(u[:]); err != nil {
		panic("alerts: crypto/rand failed: " + err.Error())
	}
	u[6] = u[6]&0x0f | 0x40
	u[8] = u[8]&0x3f | 0x80
	return u
}
