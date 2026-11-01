package main

import (
	"encoding/json"
	"math"
	"testing"
)

func totalStats(p50, p95 float64) map[string]*Stats {
	return map[string]*Stats{"total": {P50: p50, P95: p95}}
}

func TestBenchdiffMetrics(t *testing.T) {
	tests := []struct {
		name string
		run  Run
		want []benchdiffMetric
	}{
		{"webgl contexts lost yields nothing",
			Run{Scenario: "flood", WallMs: 1000, Bytes: 1 << 20, WebGLContextsLost: 1, Latency: totalStats(1, 2)}, nil},
		{"latency only for non-flood",
			Run{Scenario: "echo", WallMs: 1000, Bytes: 1 << 20, Latency: totalStats(1, 2)},
			[]benchdiffMetric{
				{"g", "total", "p50", "ms", "lower", 1},
				{"g", "total", "p95", "ms", "lower", 2},
			}},
		{"flood adds MiB/s",
			Run{Scenario: "flood", WallMs: 500, Bytes: 4 << 20, Latency: totalStats(1, 2)},
			[]benchdiffMetric{
				{"g", "total", "p50", "ms", "lower", 1},
				{"g", "total", "p95", "ms", "lower", 2},
				{"g", "throughput", "", "MiB/s", "higher", 8},
			}},
		{"flood without wall time has no throughput",
			Run{Scenario: "flood", Bytes: 4 << 20, Latency: totalStats(1, 2)},
			[]benchdiffMetric{
				{"g", "total", "p50", "ms", "lower", 1},
				{"g", "total", "p95", "ms", "lower", 2},
			}},
		{"no total latency, no wall time", Run{Scenario: "flood"}, nil},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := benchdiffMetrics(tt.run, "g")
			if len(got) != len(tt.want) {
				t.Fatalf("got %+v, want %+v", got, tt.want)
			}
			for i := range got {
				w := tt.want[i]
				if got[i].Group != w.Group || got[i].Name != w.Name || got[i].Measure != w.Measure ||
					got[i].Unit != w.Unit || got[i].Better != w.Better || math.Abs(got[i].Value-w.Value) > 1e-9 {
					t.Errorf("metric %d = %+v, want %+v", i, got[i], w)
				}
			}
		})
	}
}

func sampleResults() Results {
	return Results{
		Env: Env{GitSHA: "abc"},
		Runs: []Run{
			{Engine: "xterm", Scenario: "echo", Sessions: 16, Latency: totalStats(1, 2)},
			{Engine: "ghostty", Scenario: "flood", Sessions: 4, WallMs: 1000, Bytes: 1 << 20, Latency: totalStats(3, 4)},
		},
	}
}

func TestBenchdiffGroupsByEngineScenarioSessions(t *testing.T) {
	f := sampleResults().benchdiff()
	if f.Schema != 1 {
		t.Errorf("schema = %d, want 1", f.Schema)
	}
	groups := map[string]bool{}
	for _, m := range f.Metrics {
		groups[m.Group] = true
	}
	for _, g := range []string{"xterm/echo/16", "ghostty/flood/4"} {
		if !groups[g] {
			t.Errorf("missing group %q in %v", g, groups)
		}
	}
	if f.Env["sha"] != "abc" {
		t.Errorf("env sha = %q", f.Env["sha"])
	}
}

func TestBenchdiffByEngine(t *testing.T) {
	files := sampleResults().benchdiffByEngine()
	if len(files) != 2 {
		t.Fatalf("got %d engines, want 2", len(files))
	}
	for engine, group := range map[string]string{"xterm": "echo/16", "ghostty": "flood/4"} {
		f, ok := files[engine]
		if !ok {
			t.Fatalf("missing engine %q", engine)
		}
		if f.Schema != 1 {
			t.Errorf("%s schema = %d, want 1", engine, f.Schema)
		}
		for _, m := range f.Metrics {
			if m.Group != group {
				t.Errorf("%s group = %q, want %q", engine, m.Group, group)
			}
		}
	}
}

func TestBenchdiffEmptyMetricsMarshalAsArray(t *testing.T) {
	lost := Results{Runs: []Run{{Engine: "xterm-webgl", Scenario: "echo", Sessions: 1, WebGLContextsLost: 2, Latency: totalStats(1, 2)}}}
	for name, f := range map[string]benchdiffFile{
		"no runs":       Results{}.benchdiff(),
		"all runs lost": lost.benchdiff(),
		"by engine":     lost.benchdiffByEngine()["xterm-webgl"],
	} {
		data, err := json.Marshal(f.Metrics)
		if err != nil {
			t.Fatal(err)
		}
		if string(data) != "[]" {
			t.Errorf("%s: metrics JSON = %s, want []", name, data)
		}
	}
}
