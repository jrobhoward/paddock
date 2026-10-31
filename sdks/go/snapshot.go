package bulkhead

import (
	"fmt"

	"github.com/fxamacker/cbor/v2"
)

// A snapshot is GUEST_FORMATS.md's CBOR array `[format_version, state]`, as
// the Rust SDK writes it, so state can cross from an app in one language to
// its next version in another.

// EncodeSnapshot writes state as a snapshot of format version.
func EncodeSnapshot(version uint32, state any) ([]byte, error) {
	out, err := MarshalCBOR([]any{version, state})
	if err != nil {
		return nil, fmt.Errorf("snapshot encode: %w", err)
	}
	return out, nil
}

// SnapshotState is a snapshot's state, not yet decoded: decode it with
// [SnapshotState.Decode] when the version is the app's own, or migrate it.
type SnapshotState []byte

// Decode decodes the state into v.
func (s SnapshotState) Decode(v any) error {
	return UnmarshalCBOR(s, v)
}

// DecodeSnapshot reads a snapshot's format version and its state.
func DecodeSnapshot(snapshot []byte) (uint32, SnapshotState, error) {
	var parts []cbor.RawMessage
	if err := cbor.Unmarshal(snapshot, &parts); err != nil || len(parts) != 2 {
		return 0, nil, fmt.Errorf("not a snapshot: %v", err)
	}
	var version uint32
	if err := cbor.Unmarshal(parts[0], &version); err != nil {
		return 0, nil, fmt.Errorf("not a snapshot: %w", err)
	}
	return version, SnapshotState(parts[1]), nil
}

// LoadSnapshot decodes a snapshot of format version into v, and fails for
// another version.
func LoadSnapshot(snapshot []byte, version uint32, v any) error {
	found, state, err := DecodeSnapshot(snapshot)
	if err != nil {
		return err
	}
	if found != version {
		return fmt.Errorf("cannot read snapshot format %d (this version writes format %d)", found, version)
	}
	if err := state.Decode(v); err != nil {
		return fmt.Errorf("snapshot format %d: %w", found, err)
	}
	return nil
}
