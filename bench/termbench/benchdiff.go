package main

import "fmt"

// benchdiffMetric is one row of tstapler/benchdiff's canonical JSON; identity
// is (group, name, measure).
type benchdiffMetric struct {
	Group   string  `json:"group"`
	Name    string  `json:"name"`
	Measure string  `json:"measure,omitempty"`
	Unit    string  `json:"unit"`
	Better  string  `json:"better"`
	Value   float64 `json:"value"`
}

type benchdiffFile struct {
	Schema  int               `json:"schema"`
	Env     map[string]string `json:"env"`
	Metrics []benchdiffMetric `json:"metrics"`
}

// benchdiff converts results to benchdiff's canonical JSON. Only point values
// are emitted: per-chunk samples queue behind one another, so a significance
// test over them would overstate confidence. Runs where Chrome evicted WebGL
// contexts are left out; they are not clean measurements.
func (r Results) benchdiff() benchdiffFile {
	f := benchdiffFile{Schema: 1, Metrics: []benchdiffMetric{}, Env: map[string]string{
		"sha": r.Env.GitSHA, "cpu": r.Env.CPU, "chrome": r.Env.Chrome,
		"xterm": r.Env.XtermVer, "ghostty-web": r.Env.GhosttyVer,
	}}
	for _, run := range r.Runs {
		if run.WebGLContextsLost > 0 {
			continue
		}
		group := fmt.Sprintf("%s/%s/%d", run.Engine, run.Scenario, run.Sessions)
		add := func(name, measure, unit, better string, v float64) {
			f.Metrics = append(f.Metrics, benchdiffMetric{group, name, measure, unit, better, v})
		}
		if t := run.Latency["total"]; t != nil {
			add("total", "p50", "ms", "lower", t.P50)
			add("total", "p95", "ms", "lower", t.P95)
			add("total", "p99", "ms", "lower", t.P99)
		}
		if t := run.Latency["render"]; t != nil {
			add("render", "p50", "ms", "lower", t.P50)
		}
		if run.Scenario == "flood" && run.WallMs > 0 {
			add("throughput", "", "MiB/s", "higher", float64(run.Bytes)/(1<<20)/(run.WallMs/1000))
		}
	}
	return f
}
