package main

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"connectrpc.com/connect"
	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// Daemon is an isolated tymuxd (private sockets and XDG dirs), like the
// tymux-vs-tmux harness uses.
type Daemon struct {
	cmd    *exec.Cmd
	dir    string
	Client tymuxv1connect.TymuxServiceClient
}

func startDaemon(bin string) (*Daemon, error) {
	dir, err := os.MkdirTemp("", "tb-") // short: sun_path is 108 bytes
	if err != nil {
		return nil, err
	}
	sock := filepath.Join(dir, "tymuxd", "t.sock")
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	d := &Daemon{dir: dir}
	d.cmd = exec.Command(bin)
	d.cmd.Env = append(os.Environ(),
		"TYMUXD_SOCKET_PATH="+sock,
		"TYMUXD_ADDR=127.0.0.1:"+strconv.Itoa(port),
		"XDG_DATA_HOME="+filepath.Join(dir, "data"),
		"XDG_STATE_HOME="+filepath.Join(dir, "state"),
	)
	d.cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := d.cmd.Start(); err != nil {
		return nil, fmt.Errorf("start tymuxd: %w", err)
	}
	d.Client = tymuxv1connect.NewTymuxServiceClient(udsdialer.DialUnixHTTPClient(sock), "http://unix", connect.WithGRPC())
	err = waitFor(func() bool {
		_, err := d.Client.ListSessions(context.Background(), connect.NewRequest(&v1.ListSessionsRequest{}))
		return err == nil
	}, 10*time.Second)
	if err != nil {
		d.Stop()
	}
	return d, err
}

func (d *Daemon) Stop() {
	if d.cmd != nil && d.cmd.Process != nil {
		_ = d.cmd.Process.Kill()
		_ = d.cmd.Wait()
	}
	os.RemoveAll(d.dir)
}

// NewPane runs script under sh and returns the session and pane ids. The
// script is gated on a first line of stdin so the harness can attach before
// any output happens.
func (d *Daemon) NewPane(name, script string) (session, pane string, err error) {
	return d.create(name, "/bin/sh", "-c", "stty -echo; read _; "+script)
}

// NewEchoPane runs cat with the tty's own echo on, so a keystroke comes back
// as pty output without waiting for a newline.
func (d *Daemon) NewEchoPane(name string) (session, pane string, err error) {
	return d.create(name, "/bin/cat")
}

func (d *Daemon) create(name, command string, args ...string) (string, string, error) {
	r, err := d.Client.CreateSession(context.Background(), connect.NewRequest(&v1.CreateSessionRequest{
		Name: name, Command: command, Args: args, Cwd: "/tmp",
	}))
	if err != nil {
		return "", "", err
	}
	if ids := flattenPaneIDs(r.Msg.GetWindows()[0].GetLayout()); len(ids) > 0 {
		return r.Msg.Id, ids[0], nil
	}
	return "", "", fmt.Errorf("session %s has no pane", r.Msg.Id)
}

func flattenPaneIDs(l *v1.Layout) []string {
	if l == nil {
		return nil
	}
	switch n := l.GetNode().(type) {
	case *v1.Layout_Pane:
		return []string{n.Pane.GetId()}
	case *v1.Layout_Split:
		var ids []string
		for _, c := range n.Split.GetChildren() {
			ids = append(ids, flattenPaneIDs(c.GetLayout())...)
		}
		return ids
	}
	return nil
}

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
		time.Sleep(time.Millisecond)
	}
	return fmt.Errorf("timed out after %s", timeout)
}
