package main

import (
	"math"
	"sort"
	"time"
)

// Stats summarizes one bucket of latency samples, in milliseconds.
type Stats struct {
	N   int     `json:"n"`
	Min float64 `json:"min_ms"`
	P50 float64 `json:"p50_ms"`
	P95 float64 `json:"p95_ms"`
	Max float64 `json:"max_ms"`
}

func summarize(samples []time.Duration) Stats {
	if len(samples) == 0 {
		return Stats{}
	}
	s := append([]time.Duration(nil), samples...)
	sort.Slice(s, func(i, j int) bool { return s[i] < s[j] })
	ms := func(d time.Duration) float64 { return float64(d) / float64(time.Millisecond) }
	// Nearest-rank percentile.
	at := func(p float64) float64 {
		i := int(math.Ceil(p*float64(len(s)))) - 1
		return ms(s[max(i, 0)])
	}
	return Stats{N: len(s), Min: ms(s[0]), P50: at(0.50), P95: at(0.95), Max: ms(s[len(s)-1])}
}
