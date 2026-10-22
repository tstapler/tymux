// Command bench compares tymux against tmux on session create/kill latency as
// the number of live sessions grows, and writes the results as JSON.
//
//	go run ./bench --tymuxd target/release/tymuxd --out docs/benchmarks/results.json
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"strings"
	"time"
)

// Bucket is the latency of the creates that ran while the live session count
// was in [From, To).
type Bucket struct {
	From int `json:"live_from"`
	To   int `json:"live_to"`
	Stats
}

type BackendResult struct {
	Name         string   `json:"name"`
	Create       []Bucket `json:"create"`
	Kill         Stats    `json:"kill"`
	ServerRSSKiB int      `json:"server_rss_kib_at_max_sessions"`
	RSSPerKiB    float64  `json:"server_rss_kib_per_session"`
}

type Env struct {
	GeneratedAt string `json:"generated_at"`
	GitSHA      string `json:"tymux_git_sha"`
	TmuxVersion string `json:"tmux_version"`
	Kernel      string `json:"kernel"`
	CPU         string `json:"cpu"`
	Cores       int    `json:"cores"`
	Sessions    int    `json:"sessions"`
	BucketSize  int    `json:"bucket_size"`
	SessionCmd  string `json:"session_command"`
}

type Results struct {
	Env      Env             `json:"env"`
	Backends []BackendResult `json:"backends"`
}

func main() {
	tymuxd := flag.String("tymuxd", "target/release/tymuxd", "path to the tymuxd binary")
	tmux := flag.String("tmux", "tmux", "path to the tmux binary")
	sessions := flag.Int("sessions", 600, "sessions to create per backend")
	bucket := flag.Int("bucket", 100, "report create latency per this many live sessions")
	quick := flag.Bool("quick", false, "smoke-test sizes (100 sessions, buckets of 25)")
	out := flag.String("out", "", "write results JSON here (default: print the table only)")
	flag.Parse()
	if *quick {
		*sessions, *bucket = 100, 25
	}
	if *sessions%*bucket != 0 {
		fatalf("--sessions (%d) must be a multiple of --bucket (%d)", *sessions, *bucket)
	}

	backends := []Backend{&tymuxAPI{bin: *tymuxd}, &tmuxCLI{bin: *tmux}, &tmuxControl{tmuxCLI: tmuxCLI{bin: *tmux}}}
	res := Results{Env: collectEnv(*tmux, *sessions, *bucket)}

	// Ctrl-C must still tear servers down; they are real child processes.
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt)
	var current Backend
	go func() {
		<-sig
		if current != nil {
			current.Stop()
		}
		os.Exit(130)
	}()

	for _, b := range backends {
		current = b
		r, err := runBackend(b, *sessions, *bucket)
		if err != nil {
			b.Stop()
			fatalf("%s: %v", b.Name(), err)
		}
		res.Backends = append(res.Backends, r)
	}
	current = nil

	printTable(res)
	if *out != "" {
		data, _ := json.MarshalIndent(res, "", "  ")
		if err := os.WriteFile(*out, append(data, '\n'), 0o644); err != nil {
			fatalf("write %s: %v", *out, err)
		}
		fmt.Fprintf(os.Stderr, "wrote %s\n", *out)
	}
}

func runBackend(b Backend, n, bucketSize int) (BackendResult, error) {
	fmt.Fprintf(os.Stderr, "== %s\n", b.Name())
	dir, err := os.MkdirTemp("", "tb-") // short: sun_path is 108 bytes
	if err != nil {
		return BackendResult{}, err
	}
	defer os.RemoveAll(dir)
	if err := b.Start(dir); err != nil {
		return BackendResult{}, err
	}
	defer b.Stop()

	r := BackendResult{Name: b.Name()}
	ids := make([]string, 0, n)
	var samples []time.Duration
	for i := 0; i < n; i++ {
		t := time.Now()
		id, err := b.Create(i)
		d := time.Since(t)
		if err != nil {
			return r, fmt.Errorf("create #%d: %w", i, err)
		}
		ids = append(ids, id)
		samples = append(samples, d)
		if len(samples) == bucketSize {
			r.Create = append(r.Create, Bucket{From: i + 1 - bucketSize, To: i + 1, Stats: summarize(samples)})
			samples = samples[:0]
		}
	}
	r.ServerRSSKiB = rssKiB(b.ServerPID())
	r.RSSPerKiB = float64(r.ServerRSSKiB) / float64(n)

	kills := make([]time.Duration, 0, n)
	for _, id := range ids {
		t := time.Now()
		if err := b.Kill(id); err != nil {
			return r, fmt.Errorf("kill %s: %w", id, err)
		}
		kills = append(kills, time.Since(t))
	}
	r.Kill = summarize(kills)
	return r, nil
}

func printTable(res Results) {
	fmt.Printf("\ncreate latency p50 / p95 (ms) by live session count; %d sessions, `%s`\n\n", res.Env.Sessions, res.Env.SessionCmd)
	fmt.Printf("%-22s", "backend")
	for _, bk := range res.Backends[0].Create {
		fmt.Printf(" %13s", fmt.Sprintf("%d-%d", bk.From, bk.To))
	}
	fmt.Printf(" %13s %10s\n", "kill", "KiB/sess")
	for _, r := range res.Backends {
		fmt.Printf("%-22s", r.Name)
		for _, bk := range r.Create {
			fmt.Printf(" %13s", fmt.Sprintf("%.2f/%.2f", bk.P50, bk.P95))
		}
		fmt.Printf(" %13s %10.0f\n", fmt.Sprintf("%.2f/%.2f", r.Kill.P50, r.Kill.P95), r.RSSPerKiB)
	}
}

func collectEnv(tmuxBin string, sessions, bucket int) Env {
	e := Env{
		GeneratedAt: time.Now().UTC().Format(time.RFC3339),
		Cores:       runtime.NumCPU(),
		Sessions:    sessions,
		BucketSize:  bucket,
		SessionCmd:  sessionCmd,
		GitSHA:      run("git", "rev-parse", "--short", "HEAD"),
		TmuxVersion: run(tmuxBin, "-V"),
		Kernel:      run("uname", "-sr"),
	}
	if data, err := os.ReadFile("/proc/cpuinfo"); err == nil {
		for _, line := range strings.Split(string(data), "\n") {
			if k, v, ok := strings.Cut(line, ":"); ok && strings.TrimSpace(k) == "model name" {
				e.CPU = strings.TrimSpace(v)
				break
			}
		}
	}
	return e
}

func run(name string, args ...string) string {
	out, err := exec.Command(name, args...).Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

func fatalf(format string, a ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", a...)
	os.Exit(1)
}
