// Package loglevel holds the lowest level the host emits, which the
// `log-level` host event sets and the log package reads.
package loglevel

// Min is the lowest level the host emits, as a rank from 0 (trace) to 5
// (critical). Zero lets every record through to the host, which filters
// anyway. An app is driven one callback at a time, so it needs no lock.
var Min uint8

// Set applies a `log-level` host event. An unknown level lets every record
// through.
func Set(body []byte) {
	switch string(body) {
	case "debug":
		Min = 1
	case "info":
		Min = 2
	case "warn":
		Min = 3
	case "error":
		Min = 4
	case "critical":
		Min = 5
	default:
		Min = 0
	}
}
