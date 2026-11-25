package contract

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"syscall"
	"testing"
	"time"
)

const (
	// testAPIKey is the key every server started by the harness accepts.
	testAPIKey = "contract-test-key"
	// testMaxUploadBytes is MAX_UPLOAD_BYTES for those servers (1 MiB).
	testMaxUploadBytes = 1 << 20

	readyTimeout = 30 * time.Second
	readyPoll    = 100 * time.Millisecond
	stopGrace    = 10 * time.Second
)

// target is the service the live cases talk to, set by TestMain.
var target struct {
	mode    string // "binary" or "image"
	baseURL string
	apiKey  string
	bin     string // CONVERTD_BIN in binary mode
}

// requireFake skips a case that needs the fake engine, which only a
// test-hooks binary started by the harness has.
func requireFake(t *testing.T) {
	t.Helper()
	if target.mode != "binary" {
		t.Skipf("needs the fake engine (binary mode); running in %s mode", target.mode)
	}
}

// server is one convertd serve process started by the harness.
type server struct {
	baseURL string
	cmd     *exec.Cmd
	logPath string
	dir     string // holds TMP_DIR and the log
	exited  chan struct{}
	waitErr error
}

// serverEnv is the environment of a harness server. The process does not
// inherit the caller's environment, so a stray API_KEY or TEST_ENGINE cannot
// change it; extraEnv entries come last and win.
func serverEnv(port int, tmpDir string, extraEnv []string) []string {
	env := []string{
		"PATH=" + os.Getenv("PATH"),
		"HOST=127.0.0.1",
		"PORT=" + strconv.Itoa(port),
		"API_KEY=" + testAPIKey,
		"TMP_DIR=" + tmpDir,
		"MAX_UPLOAD_BYTES=" + strconv.Itoa(testMaxUploadBytes),
		"LOG_MODE=debug",
		"TEST_ENGINE=fake",
	}
	return append(env, extraEnv...)
}

// startServer starts `bin serve` on a free loopback port and waits until
// /readyz answers 200. Its stderr and stdout go to a log file.
func startServer(bin string, extraEnv ...string) (*server, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	dir, err := os.MkdirTemp("", "rs-convert-004-contract-")
	if err != nil {
		return nil, fmt.Errorf("failed to create server dir: %w", err)
	}
	tmpDir := filepath.Join(dir, "tmp")
	logPath := filepath.Join(dir, "convertd.log")
	logFile, err := os.Create(logPath)
	if err != nil {
		_ = os.RemoveAll(dir)
		return nil, fmt.Errorf("failed to create server log: %w", err)
	}

	cmd := exec.Command(bin, "serve")
	cmd.Env = serverEnv(port, tmpDir, extraEnv)
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	if err := cmd.Start(); err != nil {
		_ = logFile.Close()
		_ = os.RemoveAll(dir)
		return nil, fmt.Errorf("failed to start %s: %w", bin, err)
	}
	s := &server{
		baseURL: "http://127.0.0.1:" + strconv.Itoa(port),
		cmd:     cmd,
		logPath: logPath,
		dir:     dir,
		exited:  make(chan struct{}),
	}
	go func() {
		s.waitErr = cmd.Wait()
		_ = logFile.Close()
		close(s.exited)
	}()

	if err := waitReady(s.baseURL, readyTimeout, s.exited); err != nil {
		_ = s.stop()
		logs := s.logs()
		s.remove()
		return nil, fmt.Errorf("failed to start server: %w\n--- server log ---\n%s", err, logs)
	}
	return s, nil
}

// startTestServer starts a server of the test's own, for a case that needs
// other settings, and stops it when the test ends. The log is printed if the
// test failed.
func startTestServer(t *testing.T, extraEnv ...string) *server {
	t.Helper()
	if target.mode != "binary" {
		t.Skipf("needs a server of its own (binary mode); running in %s mode", target.mode)
	}
	s, err := startServer(target.bin, extraEnv...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := s.stop(); err != nil {
			t.Errorf("failed to stop server: %v", err)
		}
		if t.Failed() {
			t.Logf("--- server log ---\n%s", s.logs())
		}
		s.remove()
	})
	return s
}

// stop sends SIGTERM, waits up to stopGrace, then sends SIGKILL. The log
// stays readable until remove.
func (s *server) stop() error {
	select {
	case <-s.exited:
		return s.exitError()
	default:
	}
	if err := s.cmd.Process.Signal(syscall.SIGTERM); err != nil && !errors.Is(err, os.ErrProcessDone) {
		return fmt.Errorf("failed to send SIGTERM: %w", err)
	}
	select {
	case <-s.exited:
		return s.exitError()
	case <-time.After(stopGrace):
		_ = s.cmd.Process.Kill()
		<-s.exited
		return fmt.Errorf("server did not exit within %s of SIGTERM and was killed", stopGrace)
	}
}

// remove deletes the server's directory: its TMP_DIR and its log.
func (s *server) remove() {
	_ = os.RemoveAll(s.dir)
}

func (s *server) exitError() error {
	if s.waitErr == nil {
		return nil
	}
	return fmt.Errorf("server exited with an error: %w", s.waitErr)
}

// logs returns what the server wrote so far.
func (s *server) logs() string {
	b, err := os.ReadFile(s.logPath)
	if err != nil {
		return fmt.Sprintf("(failed to read log: %v)", err)
	}
	return string(b)
}

// freePort asks the kernel for a free loopback port. Another process may take
// it before the server binds; the readiness wait then fails loudly.
func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, fmt.Errorf("failed to find a free port: %w", err)
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

// waitReady polls GET /readyz until it answers 200, the process exits
// (exited is closed; nil means it is not ours) or timeout passes.
func waitReady(baseURL string, timeout time.Duration, exited <-chan struct{}) error {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	client := &http.Client{Timeout: 2 * time.Second}
	ticker := time.NewTicker(readyPoll)
	defer ticker.Stop()
	last := "no answer"
	for {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, baseURL+"/readyz", nil)
		if err != nil {
			return fmt.Errorf("failed to build readiness request: %w", err)
		}
		resp, err := client.Do(req)
		if err == nil {
			_ = resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				return nil
			}
			last = resp.Status
		} else {
			last = err.Error()
		}
		select {
		case <-exited:
			return errors.New("server exited before it was ready")
		case <-ctx.Done():
			return fmt.Errorf("server not ready after %s (last: %s)", timeout, last)
		case <-ticker.C:
		}
	}
}
