package main

import (
	"encoding/binary"
	"errors"
)

// The one message shape ping-pong uses, `{ "n": <unsigned> }`, as the Rust
// side's ciborium writes it: a map of one entry (0xa1), the text key "n"
// (0x61 0x6e), then an unsigned integer. A larger app would use a CBOR
// library; this keeps the example free of dependencies.

var key = []byte{0xa1, 0x61, 'n'}

func encodeN(n uint64) []byte {
	b := append([]byte(nil), key...)
	switch {
	case n < 24:
		return append(b, byte(n))
	case n <= 0xff:
		return append(b, 0x18, byte(n))
	case n <= 0xffff:
		return binary.BigEndian.AppendUint16(append(b, 0x19), uint16(n))
	case n <= 0xffffffff:
		return binary.BigEndian.AppendUint32(append(b, 0x1a), uint32(n))
	default:
		return binary.BigEndian.AppendUint64(append(b, 0x1b), n)
	}
}

func decodeN(b []byte) (uint64, error) {
	if len(b) < len(key)+1 || string(b[:len(key)]) != string(key) {
		return 0, errors.New("not a CBOR map of one key \"n\"")
	}
	head, rest := b[len(key)], b[len(key)+1:]
	if head>>5 != 0 {
		return 0, errors.New("n is not an unsigned integer")
	}
	var want int
	switch info := head & 0x1f; {
	case info < 24:
		if len(rest) != 0 {
			return 0, errors.New("trailing bytes")
		}
		return uint64(info), nil
	case info <= 27:
		want = 1 << (info - 24)
	default:
		return 0, errors.New("bad length for n")
	}
	if len(rest) != want {
		return 0, errors.New("bad length for n")
	}
	var n uint64
	for _, c := range rest {
		n = n<<8 | uint64(c)
	}
	return n, nil
}
