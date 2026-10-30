// Package log writes records through wasi:logging. Declare the
// `wasi:logging` capability in the manifest; records carry the app, version
// and pid, and the host caps their level and rate.
//
// The host tells the instance its effective level with a `log-level` host
// event, which the SDK handles itself. From then on the functions here skip
// records below that level before formatting them. Until the event arrives
// (during Init), every record goes to the host, which filters it.
package log

import (
	"fmt"

	"github.com/jrobhoward/paddock/sdks/go/internal/gen/wasi/logging/logging"
	"github.com/jrobhoward/paddock/sdks/go/internal/loglevel"
)

// Level is a record's level.
type Level uint8

// The levels, lowest first.
const (
	Trace Level = iota
	Debug
	Info
	Warn
	Error
	Critical
)

var names = [...]string{"trace", "debug", "info", "warn", "error", "critical"}

// String is the level's name in lower case.
func (l Level) String() string {
	if int(l) < len(names) {
		return names[l]
	}
	return "unknown"
}

// ParseLevel reads a level's name.
func ParseLevel(name string) (Level, bool) {
	for i, n := range names {
		if n == name {
			return Level(i), true
		}
	}
	return Trace, false
}

// Enabled reports whether the host would emit a record at level.
func Enabled(level Level) bool {
	return uint8(level) >= loglevel.Min
}

// Write writes one record with context `app`, unless the host would drop
// it.
func Write(level Level, message string) {
	if !Enabled(level) {
		return
	}
	logging.Log(logging.Level(level), "app", message)
}

func logf(level Level, format string, args ...any) {
	if Enabled(level) {
		logging.Log(logging.Level(level), "app", fmt.Sprintf(format, args...))
	}
}

// Tracef logs at trace, formatting only if the host would emit it.
func Tracef(format string, args ...any) { logf(Trace, format, args...) }

// Debugf logs at debug.
func Debugf(format string, args ...any) { logf(Debug, format, args...) }

// Infof logs at info.
func Infof(format string, args ...any) { logf(Info, format, args...) }

// Warnf logs at warn.
func Warnf(format string, args ...any) { logf(Warn, format, args...) }

// Errorf logs at error.
func Errorf(format string, args ...any) { logf(Error, format, args...) }

// Criticalf logs at critical.
func Criticalf(format string, args ...any) { logf(Critical, format, args...) }
