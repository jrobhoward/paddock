// Package formats reads the examples in docs/GUEST_FORMATS.md, for the
// SDK's tests.
package formats

import (
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// Example is the body of the fenced block "```<kind> <name>" in
// GUEST_FORMATS.md: bytes from hex for cbor-hex, the text otherwise.
func Example(kind, name string) ([]byte, error) {
	_, here, _, _ := runtime.Caller(0)
	doc, err := os.ReadFile(filepath.Join(filepath.Dir(here), "../../../../docs/GUEST_FORMATS.md"))
	if err != nil {
		return nil, err
	}
	fence := "```" + kind + " " + name + "\n"
	_, rest, found := strings.Cut(string(doc), fence)
	if !found {
		return nil, fmt.Errorf("no example %s %s", kind, name)
	}
	text, _, _ := strings.Cut(rest, "```")
	if kind != "cbor-hex" {
		return []byte(strings.TrimSpace(text)), nil
	}
	return hex.DecodeString(strings.Join(strings.Fields(text), ""))
}
