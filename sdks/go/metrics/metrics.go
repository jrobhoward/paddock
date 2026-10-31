// Package metrics is counters, gauges and histograms through
// bulkhead:metrics, exported by the node in Prometheus text. Declare the
// `bulkhead:metrics` capability in the manifest. Updates never fail: a
// metric that could be made can always be updated.
package metrics

import (
	"errors"

	"github.com/jrobhoward/bulkhead/sdks/go/internal/abi"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/bulkhead/metrics/metrics"
	"go.bytecodealliance.org/cm"
)

var (
	// ErrAccessDenied: the capability is not granted.
	ErrAccessDenied = errors.New("access denied")
	// ErrLimitReached: the app has as many series as the node allows.
	ErrLimitReached = errors.New("series limit reached")
)

// InvalidError: a name, label or bucket the host refuses, or a name
// registered before as another kind or with other labels' names.
type InvalidError struct{ Msg string }

func (e *InvalidError) Error() string { return "invalid: " + e.Msg }

// Label is a label's name and value.
type Label struct{ Name, Value string }

func register(kind metrics.Kind, name, help string, labels []Label, buckets []float64) (uint32, error) {
	pairs := make([][2]string, 0, len(labels))
	for _, l := range labels {
		pairs = append(pairs, [2]string{l.Name, l.Value})
	}
	r := metrics.Register(kind, name, help, cm.ToList(pairs), cm.ToList(buckets))
	if e := r.Err(); e != nil {
		switch {
		case e.AccessDenied():
			return 0, ErrAccessDenied
		case e.LimitReached():
			return 0, ErrLimitReached
		case e.Invalid() != nil:
			return 0, &InvalidError{Msg: abi.String(*e.Invalid())}
		default:
			return 0, &InvalidError{Msg: "unknown metric"}
		}
	}
	return *r.OK(), nil
}

// Counter only goes up.
type Counter struct{ id uint32 }

// NewCounter makes (or finds) the counter name with these labels.
func NewCounter(name, help string, labels ...Label) (Counter, error) {
	id, err := register(metrics.KindCounter, name, help, labels, nil)
	return Counter{id}, err
}

// Inc adds one.
func (c Counter) Inc() { c.Add(1) }

// Add adds n, which must not be negative.
func (c Counter) Add(n float64) { metrics.Add(c.id, n) }

// Gauge goes up and down.
type Gauge struct{ id uint32 }

// NewGauge makes (or finds) the gauge name with these labels.
func NewGauge(name, help string, labels ...Label) (Gauge, error) {
	id, err := register(metrics.KindGauge, name, help, labels, nil)
	return Gauge{id}, err
}

// Set sets the value.
func (g Gauge) Set(v float64) { metrics.Set(g.id, v) }

// Add adds delta, which may be negative.
func (g Gauge) Add(delta float64) { metrics.Add(g.id, delta) }

// Histogram counts observations into buckets.
type Histogram struct{ id uint32 }

// NewHistogram makes (or finds) the histogram name with these labels and
// bucket bounds.
func NewHistogram(name, help string, buckets []float64, labels ...Label) (Histogram, error) {
	id, err := register(metrics.KindHistogram, name, help, labels, buckets)
	return Histogram{id}, err
}

// Observe records one value.
func (h Histogram) Observe(v float64) { metrics.Observe(h.id, v) }
