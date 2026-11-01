package main

import (
	"math"
	"sort"
)

// Stats summarizes latency samples, in milliseconds.
type Stats struct {
	N    int     `json:"n"`
	Min  float64 `json:"min_ms"`
	Mean float64 `json:"mean_ms"`
	P50  float64 `json:"p50_ms"`
	P95  float64 `json:"p95_ms"`
	P99  float64 `json:"p99_ms"`
	Max  float64 `json:"max_ms"`
}

// summarize returns nil for no samples so absent metrics are omitted, not zero.
func summarize(ms []float64) *Stats {
	if len(ms) == 0 {
		return nil
	}
	s := append([]float64(nil), ms...)
	sort.Float64s(s)
	var sum float64
	for _, v := range s {
		sum += v
	}
	// Nearest-rank percentile.
	at := func(p float64) float64 {
		return s[max(int(math.Ceil(p*float64(len(s))))-1, 0)]
	}
	return &Stats{N: len(s), Min: s[0], Mean: sum / float64(len(s)), P50: at(0.50), P95: at(0.95), P99: at(0.99), Max: s[len(s)-1]}
}
