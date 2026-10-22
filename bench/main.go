// Command bench compares tymux against tmux and writes the results as JSON.
//
//	scripts/bench.sh --out docs/benchmarks/results.json
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

// BackendResult holds every scenario for one backend; scenarios a backend
// can't run are omitted (nil), not zero.
type BackendResult struct {
	Name   string   `json:"name"`
	Create []Bucket `json:"create"`
	Kill   Stats    `json:"kill"`
	// List and Capture are measured with the maximum number of live sessions.
	List    Stats  `json:"list_at_max_sessions"`
	Capture *Stats `json:"capture_at_max_sessions,omitempty"`
	// Echo is keystroke -> echoed output over a persistent connection.
	Echo *Stats `json:"keystroke_echo,omitempty"`
	// ColdStart is server launch until the first request is answered.
	ColdStart *Stats `json:"cold_start,omitempty"`
	// Reattach is dropping the client and getting it streaming again.
	Reattach *Reattach `json:"reattach,omitempty"`
	// Memory is the server's RSS; omitted for rows that share another row's server.
	ServerRSSKiB int     `json:"server_rss_kib_at_max_sessions,omitempty"`
	RSSPerKiB    float64 `json:"server_rss_kib_per_session,omitempty"`
}

type Reattach struct {
	Stats
	// Contiguous counts reattaches whose first chunk followed the recorded
	// seq exactly; 0 for tmux, which has no resume protocol.
	Contiguous int `json:"gap_free_resumes"`
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

type sizes struct{ sessions, bucket, echo, capture, list, cold, reattach int }

func main() {
	tymuxd := flag.String("tymuxd", "target/release/tymuxd", "path to the tymuxd binary")
	tymux := flag.String("tymux", "target/release/tymux", "path to the tymux CLI binary")
	tmux := flag.String("tmux", "tmux", "path to the tmux binary")
	quick := flag.Bool("quick", false, "smoke-test sizes (100 sessions, fewer samples)")
	out := flag.String("out", "", "write results JSON here (default: print the table only)")
	flag.Parse()

	sz := sizes{sessions: 600, bucket: 100, echo: 200, capture: 100, list: 30, cold: 20, reattach: 50}
	if *quick {
		sz = sizes{sessions: 100, bucket: 25, echo: 50, capture: 30, list: 10, cold: 5, reattach: 10}
	}

	mk := func() []Backend {
		return []Backend{
			&tymuxAPI{bin: *tymuxd},
			&tymuxCLI{tymuxAPI: tymuxAPI{bin: *tymuxd}, cli: *tymux},
			&tmuxCLI{bin: *tmux},
			&tmuxControl{tmuxCLI: tmuxCLI{bin: *tmux}},
		}
	}
	res := Results{Env: collectEnv(*tmux, sz)}

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

	for i := range mk() {
		b := mk()[i]
		current = b
		r, err := runScale(b, sz)
		if err != nil {
			b.Stop()
			fatalf("%s: %v", b.Name(), err)
		}
		if b.OwnsServer() {
			cs, err := runColdStart(mk()[i], sz.cold)
			if err != nil {
				fatalf("%s cold start: %v", b.Name(), err)
			}
			r.ColdStart = &cs
		}
		if _, ok := b.(Reattacher); ok && b.OwnsServer() {
			ra, err := runReattach(mk()[i], sz.reattach)
			if err != nil {
				fatalf("%s reattach: %v", b.Name(), err)
			}
			r.Reattach = ra
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

// supportsEcho: rows that share another row's persistent-connection path
// (the CLI forks a process per command) have no keystroke-echo number.
func supportsEcho(b Backend) bool {
	switch b.(type) {
	case *tymuxCLI, *tmuxCLI:
		return false
	}
	_, ok := b.(Echoer)
	return ok
}

func withServer(b Backend, f func(dir string) error) error {
	dir, err := os.MkdirTemp("", "tb-") // short: sun_path is 108 bytes
	if err != nil {
		return err
	}
	defer os.RemoveAll(dir)
	if err := b.Start(dir); err != nil {
		b.Stop()
		return err
	}
	defer b.Stop()
	return f(dir)
}

func runScale(b Backend, sz sizes) (BackendResult, error) {
	fmt.Fprintf(os.Stderr, "== %s\n", b.Name())
	r := BackendResult{Name: b.Name()}
	err := withServer(b, func(string) error {
		ids := make([]string, 0, sz.sessions)
		var samples []time.Duration
		for i := 0; i < sz.sessions; i++ {
			t := time.Now()
			id, err := b.Create(i)
			d := time.Since(t)
			if err != nil {
				return fmt.Errorf("create #%d: %w", i, err)
			}
			ids = append(ids, id)
			samples = append(samples, d)
			if len(samples) == sz.bucket {
				r.Create = append(r.Create, Bucket{From: i + 1 - sz.bucket, To: i + 1, Stats: summarize(samples)})
				samples = samples[:0]
			}
		}
		if b.OwnsServer() {
			r.ServerRSSKiB = rssKiB(b.ServerPID())
			r.RSSPerKiB = float64(r.ServerRSSKiB) / float64(sz.sessions)
		}

		// Everything below runs with the maximum number of sessions alive.
		var err error
		if r.List, err = timeN(sz.list, b.List); err != nil {
			return fmt.Errorf("list: %w", err)
		}
		if e, ok := b.(Echoer); ok {
			// Type into session 0 so its screen has content for the capture
			// below, whether or not this row reports echo latency.
			echo, err := e.EchoSession(ids[0])
			if err != nil {
				return fmt.Errorf("echo session: %w", err)
			}
			var samples []time.Duration
			for i := 0; i < sz.echo; i++ {
				d, err := echo.RoundTrip(byte('a' + i%26))
				if err != nil {
					echo.Close()
					return fmt.Errorf("echo: %w", err)
				}
				samples = append(samples, d)
			}
			echo.Close()
			if supportsEcho(b) {
				s := summarize(samples)
				r.Echo = &s
			}
		}
		c, err := timeN(sz.capture, func() error { _, err := b.Capture(ids[0]); return err })
		if err != nil {
			return fmt.Errorf("capture: %w", err)
		}
		r.Capture = &c

		kills := make([]time.Duration, 0, len(ids))
		for _, id := range ids {
			t := time.Now()
			if err := b.Kill(id); err != nil {
				return fmt.Errorf("kill %s: %w", id, err)
			}
			kills = append(kills, time.Since(t))
		}
		r.Kill = summarize(kills)
		return nil
	})
	return r, err
}

func timeN(n int, f func() error) (Stats, error) {
	var s []time.Duration
	for i := 0; i < n; i++ {
		t := time.Now()
		if err := f(); err != nil {
			return Stats{}, err
		}
		s = append(s, time.Since(t))
	}
	return summarize(s), nil
}

// runColdStart launches a fresh server n times and times each launch until
// the server answers.
func runColdStart(b Backend, n int) (Stats, error) {
	var s []time.Duration
	for i := 0; i < n; i++ {
		dir, err := os.MkdirTemp("", "tb-")
		if err != nil {
			return Stats{}, err
		}
		t := time.Now()
		err = b.Start(dir)
		d := time.Since(t)
		b.Stop()
		os.RemoveAll(dir)
		if err != nil {
			return Stats{}, err
		}
		s = append(s, d)
	}
	return summarize(s), nil
}

func runReattach(b Backend, n int) (*Reattach, error) {
	ra := b.(Reattacher)
	lastSeq = 0
	var out *Reattach
	err := withServer(b, func(string) error {
		id, err := ra.ReattachTarget()
		if err != nil {
			return err
		}
		// The first attach has nothing to resume from; it only learns the seq.
		if _, _, err := ra.Reattach(id); err != nil {
			return err
		}
		time.Sleep(20 * time.Millisecond)
		var s []time.Duration
		ok := 0
		for i := 0; i < n; i++ {
			d, contiguous, err := ra.Reattach(id)
			if err != nil {
				return err
			}
			s = append(s, d)
			if contiguous {
				ok++
			}
			time.Sleep(20 * time.Millisecond) // output accrues while "offline"
		}
		out = &Reattach{Stats: summarize(s), Contiguous: ok}
		return nil
	})
	return out, err
}

func printTable(res Results) {
	fmt.Printf("\n%d sessions of `%s`; ms, p50 (p95)\n\n", res.Env.Sessions, res.Env.SessionCmd)
	cell := func(s *Stats) string {
		if s == nil || s.N == 0 {
			return "-"
		}
		return fmt.Sprintf("%.2f (%.2f)", s.P50, s.P95)
	}
	fmt.Printf("%-22s %-14s %-14s %-14s %-14s %-14s %-14s %-14s %-14s\n", "backend", "create first", "create last", "kill", "list@max", "capture", "echo", "cold start", "reattach")
	for _, r := range res.Backends {
		f, l := r.Create[0].Stats, r.Create[len(r.Create)-1].Stats
		var ra *Stats
		if r.Reattach != nil {
			ra = &r.Reattach.Stats
		}
		fmt.Printf("%-22s %-14s %-14s %-14s %-14s %-14s %-14s %-14s %-14s\n", r.Name, cell(&f), cell(&l), cell(&r.Kill), cell(&r.List), cell(r.Capture), cell(r.Echo), cell(r.ColdStart), cell(ra))
	}
	fmt.Println()
	for _, r := range res.Backends {
		if r.RSSPerKiB > 0 {
			fmt.Printf("%-22s server RSS %.0f KiB per session\n", r.Name, r.RSSPerKiB)
		}
		if r.Reattach != nil && strings.HasPrefix(r.Name, "tymux") {
			fmt.Printf("%-22s gap-free resumes %d/%d (tmux has no resume protocol)\n", r.Name, r.Reattach.Contiguous, r.Reattach.N)
		}
	}
}

func collectEnv(tmuxBin string, sz sizes) Env {
	e := Env{
		GeneratedAt: time.Now().UTC().Format(time.RFC3339),
		Cores:       runtime.NumCPU(),
		Sessions:    sz.sessions,
		BucketSize:  sz.bucket,
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
