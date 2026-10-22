package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"connectrpc.com/connect"
	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// Backend is one multiplexer under test. Every backend creates a session
// running /bin/cat (blocks on the pty, no shell startup in the numbers).
type Backend interface {
	Name() string
	Start(workdir string) error
	Create(i int) (id string, err error)
	Kill(id string) error
	// ServerPID is the multiplexer server process, for RSS sampling.
	ServerPID() int
	Stop()
}

const sessionCmd = "/bin/cat"

// ---- tymuxd over gRPC ----

type tymuxAPI struct {
	bin    string
	cmd    *exec.Cmd
	client tymuxv1connect.TymuxServiceClient
}

func (b *tymuxAPI) Name() string { return "tymux (gRPC API)" }

func (b *tymuxAPI) Start(workdir string) error {
	// sun_path is limited to 108 bytes and tymuxd wants to create (mode 0700)
	// the socket's parent itself, so the dir must not exist yet.
	sockDir := filepath.Join(workdir, "tymuxd")
	sock := filepath.Join(sockDir, "t.sock")
	port, err := freePort()
	if err != nil {
		return err
	}
	b.cmd = exec.Command(b.bin)
	b.cmd.Env = append(os.Environ(),
		"TYMUXD_SOCKET_PATH="+sock,
		"TYMUXD_ADDR=127.0.0.1:"+strconv.Itoa(port),
		"XDG_DATA_HOME="+filepath.Join(workdir, "data"),
		"XDG_STATE_HOME="+filepath.Join(workdir, "state"),
	)
	b.cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := b.cmd.Start(); err != nil {
		return fmt.Errorf("start tymuxd: %w", err)
	}
	if err := waitFor(func() bool { _, err := os.Stat(sock); return err == nil }, 10*time.Second); err != nil {
		return fmt.Errorf("tymuxd socket never appeared: %w", err)
	}
	b.client = tymuxv1connect.NewTymuxServiceClient(udsdialer.DialUnixHTTPClient(sock), "http://unix", connect.WithGRPC())
	return nil
}

func (b *tymuxAPI) Create(i int) (string, error) {
	r, err := b.client.CreateSession(context.Background(), connect.NewRequest(&v1.CreateSessionRequest{
		Name: fmt.Sprintf("s%d", i), Command: sessionCmd, Cwd: "/tmp",
	}))
	if err != nil {
		return "", err
	}
	return r.Msg.Id, nil
}

func (b *tymuxAPI) Kill(id string) error {
	_, err := b.client.KillSession(context.Background(), connect.NewRequest(&v1.KillSessionRequest{SessionId: id}))
	return err
}

func (b *tymuxAPI) ServerPID() int { return b.cmd.Process.Pid }

func (b *tymuxAPI) Stop() {
	if b.cmd != nil && b.cmd.Process != nil {
		_ = b.cmd.Process.Kill()
		_ = b.cmd.Wait()
	}
}

// ---- tmux, one CLI process per command ----

type tmuxCLI struct {
	bin  string
	sock string
}

func (b *tmuxCLI) Name() string { return "tmux (CLI)" }

func (b *tmuxCLI) tmux(args ...string) *exec.Cmd {
	c := exec.Command(b.bin, append([]string{"-S", b.sock, "-f", "/dev/null"}, args...)...)
	c.Env = append(os.Environ(), "TERM=xterm-256color")
	return c
}

func (b *tmuxCLI) Start(workdir string) error {
	b.sock = filepath.Join(workdir, "tmux.sock")
	// Starts the server (the first command does) outside the timed region.
	if out, err := b.tmux("start-server", ";", "set", "-g", "exit-empty", "off").CombinedOutput(); err != nil {
		return fmt.Errorf("tmux start-server: %w: %s", err, out)
	}
	return nil
}

func (b *tmuxCLI) Create(i int) (string, error) {
	name := fmt.Sprintf("s%d", i)
	if out, err := b.tmux("new-session", "-d", "-s", name, "-x", "80", "-y", "24", sessionCmd).CombinedOutput(); err != nil {
		return "", fmt.Errorf("%w: %s", err, out)
	}
	return name, nil
}

func (b *tmuxCLI) Kill(id string) error {
	out, err := b.tmux("kill-session", "-t", "="+id).CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, out)
	}
	return nil
}

func (b *tmuxCLI) ServerPID() int { return tmuxServerPID(b) }

func (b *tmuxCLI) Stop() { _ = b.tmux("kill-server").Run() }

func tmuxServerPID(b *tmuxCLI) int {
	out, err := b.tmux("display-message", "-p", "#{pid}").Output()
	if err != nil {
		return 0
	}
	pid, _ := strconv.Atoi(strings.TrimSpace(string(out)))
	return pid
}

// ---- tmux over one persistent control-mode connection ----

// tmuxControl is the closest tmux analogue to a long-lived gRPC client: one
// connection, commands written as lines, replies framed by %begin/%end.
type tmuxControl struct {
	tmuxCLI
	ctl   *exec.Cmd
	stdin io.WriteCloser
	done  chan error
}

func (b *tmuxControl) Name() string { return "tmux (control mode)" }

func (b *tmuxControl) Start(workdir string) error {
	if err := b.tmuxCLI.Start(workdir); err != nil {
		return err
	}
	b.ctl = b.tmux("-C", "new-session", "-s", "ctl", sessionCmd)
	in, err := b.ctl.StdinPipe()
	if err != nil {
		return err
	}
	out, err := b.ctl.StdoutPipe()
	if err != nil {
		return err
	}
	b.stdin = in
	b.done = make(chan error, 16)
	if err := b.ctl.Start(); err != nil {
		return err
	}
	go func() {
		sc := bufio.NewScanner(out)
		sc.Buffer(make([]byte, 1<<20), 1<<20)
		for sc.Scan() {
			line := sc.Text()
			switch {
			case strings.HasPrefix(line, "%end "):
				b.done <- nil
			case strings.HasPrefix(line, "%error "):
				b.done <- fmt.Errorf("tmux control: %s", line)
			}
		}
	}()
	// The attach itself produces a %begin/%end pair; consume it so replies
	// to our commands line up.
	select {
	case <-b.done:
	case <-time.After(5 * time.Second):
		return fmt.Errorf("tmux control mode never answered")
	}
	return nil
}

func (b *tmuxControl) run(cmd string) error {
	if _, err := io.WriteString(b.stdin, cmd+"\n"); err != nil {
		return err
	}
	select {
	case err := <-b.done:
		return err
	case <-time.After(10 * time.Second):
		return fmt.Errorf("tmux control: timeout on %q", cmd)
	}
}

func (b *tmuxControl) Create(i int) (string, error) {
	name := fmt.Sprintf("s%d", i)
	return name, b.run(fmt.Sprintf("new-session -d -s %s -x 80 -y 24 %s", name, sessionCmd))
}

func (b *tmuxControl) Kill(id string) error { return b.run("kill-session -t =" + id) }

func (b *tmuxControl) Stop() {
	if b.stdin != nil {
		_ = b.stdin.Close()
	}
	b.tmuxCLI.Stop()
	if b.ctl != nil {
		_ = b.ctl.Wait()
	}
}

// ---- helpers ----

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func waitFor(cond func() bool, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if cond() {
			return nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	return fmt.Errorf("timed out after %s", timeout)
}

// rssKiB reads VmRSS from /proc (Linux only; 0 elsewhere).
func rssKiB(pid int) int {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return 0
	}
	for _, line := range strings.Split(string(data), "\n") {
		if rest, ok := strings.CutPrefix(line, "VmRSS:"); ok {
			f := strings.Fields(rest)
			if len(f) > 0 {
				n, _ := strconv.Atoi(f[0])
				return n
			}
		}
	}
	return 0
}
