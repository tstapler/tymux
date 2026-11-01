package main

import (
	"math"
	"reflect"
	"testing"
)

func seq(n int) []float64 {
	s := make([]float64, n)
	for i := range s {
		s[i] = float64(i + 1)
	}
	return s
}

func TestSummarizeEmpty(t *testing.T) {
	if got := summarize(nil); got != nil {
		t.Fatalf("summarize(nil) = %+v, want nil", got)
	}
	if got := summarize([]float64{}); got != nil {
		t.Fatalf("summarize(empty) = %+v, want nil", got)
	}
}

func TestSummarize(t *testing.T) {
	tests := []struct {
		name string
		in   []float64
		want Stats
	}{
		{"single", []float64{7}, Stats{N: 1, Min: 7, Mean: 7, P50: 7, P95: 7, P99: 7, Max: 7}},
		{"n=100", seq(100), Stats{N: 100, Min: 1, Mean: 50.5, P50: 50, P95: 95, P99: 99, Max: 100}},
		{"n=10", seq(10), Stats{N: 10, Min: 1, Mean: 5.5, P50: 5, P95: 10, P99: 10, Max: 10}},
		{"n=3 unsorted", []float64{3, 1, 2}, Stats{N: 3, Min: 1, Mean: 2, P50: 2, P95: 3, P99: 3, Max: 3}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := summarize(tt.in)
			if got == nil {
				t.Fatal("got nil")
			}
			if got.N != tt.want.N || got.Min != tt.want.Min || got.P50 != tt.want.P50 ||
				got.P95 != tt.want.P95 || got.P99 != tt.want.P99 || got.Max != tt.want.Max ||
				math.Abs(got.Mean-tt.want.Mean) > 1e-9 {
				t.Fatalf("got %+v, want %+v", *got, tt.want)
			}
		})
	}
}

func TestSummarizeDoesNotMutateInput(t *testing.T) {
	in := []float64{5, 3, 9, 1}
	orig := append([]float64(nil), in...)
	summarize(in)
	if !reflect.DeepEqual(in, orig) {
		t.Fatalf("input mutated: %v, want %v", in, orig)
	}
}
