//go:build contract

package contract

import (
	"bufio"
	"bytes"
	"encoding/json"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/viktordanov/convertd/client/convert"
)

// The fake-worker cases: each runs a server of its own
// with TEST_ENGINE=worker, so both pools run `convertd worker test` behind the
// real supervisor pool. The 504 then comes from the real deadline and kill,
// and the 429 from the real office queue. The log is JSON (LOG_MODE=prod), so
// the cases can read the pool's events.

func startLevelB(t *testing.T, extraEnv ...string) *server {
	t.Helper()
	env := append([]string{"TEST_ENGINE=worker", "OFFICE_WORKERS=1", "RENDER_WORKERS=1", "LOG_MODE=prod"}, extraEnv...)
	return startTestServer(t, env...)
}

// logEvents returns the JSON log lines of s whose message is msg.
func logEvents(s *server, msg string) []map[string]any {
	var events []map[string]any
	sc := bufio.NewScanner(strings.NewReader(s.logs()))
	sc.Buffer(make([]byte, 0, 64<<10), 1<<20)
	for sc.Scan() {
		var e map[string]any
		if json.Unmarshal(sc.Bytes(), &e) == nil && e["message"] == msg {
			events = append(events, e)
		}
	}
	return events
}

// waitForEvent polls the log until match finds an event or timeout passes.
func waitForEvent(s *server, msg string, timeout time.Duration, match func(map[string]any) bool) (map[string]any, bool) {
	deadline := time.Now().Add(timeout)
	for {
		for _, e := range logEvents(s, msg) {
			if match(e) {
				return e, true
			}
		}
		if time.Now().After(deadline) {
			return nil, false
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func number(v any) float64 {
	f, _ := v.(float64)
	return f
}

// Case 14, fake worker: the deadline kills the office worker, the call answers
// 504, and the pool starts a replacement.
func TestLevelBTimeoutKillsAndReplaces(t *testing.T) {
	t.Parallel()
	s := startLevelB(t)
	f := facadeFor(t, s.baseURL, testAPIKey, newTransport(t))

	start := time.Now()
	_, err := f.ConvertToPDF(testContext(t), "doc.docx", bytes.NewReader(docxWithMarker("FAKE:DELAY_MS=6000")),
		convert.Options{Timeout: 5 * time.Second})
	elapsed := time.Since(start)
	wantErr(t, err, convert.ErrTimeout)
	if elapsed < 4500*time.Millisecond || elapsed > 6*time.Second {
		t.Errorf("timeout answered after %s, want about 5 s", elapsed)
	}

	killed, ok := waitForEvent(s, "worker killed", 3*time.Second, func(e map[string]any) bool {
		return e["pool"] == "office" && e["reason"] == "timeout"
	})
	if !ok {
		t.Fatal("the server log has no office \"worker killed\" event with reason timeout")
	}
	gen := number(killed["generation"])
	ready, ok := waitForEvent(s, "worker ready", 5*time.Second, func(e map[string]any) bool {
		return e["pool"] == "office" && number(e["generation"]) > gen
	})
	if !ok {
		t.Fatalf("no office worker of a generation after %v became ready after the kill", gen)
	}
	t.Logf("killed %v generation %v (reason %v); replacement %v generation %v ready",
		killed["worker_id"], killed["generation"], killed["reason"], ready["worker_id"], ready["generation"])

	// The replacement serves the next request.
	res, err := f.ConvertToPDF(testContext(t), "doc.docx", bytes.NewReader(docx()), convert.Options{})
	if err != nil {
		t.Fatalf("request after the replacement: %v", err)
	}
	_ = res.Body.Close()
}

// Case 15b, fake worker: one office worker, a queue of one and a 1 s queue wait.
// A holds the worker; of B and C, one waits in the queue until its 1 s wait
// ends and the other finds the queue full. Both get 429; which is which is
// not asserted.
func TestLevelBQueueFull(t *testing.T) {
	t.Parallel()
	s := startLevelB(t, "QUEUE_CAPACITY=1", "QUEUE_WAIT_SECONDS=1")
	f := facadeFor(t, s.baseURL, testAPIKey, newTransport(t))
	ctx := testContext(t)
	held := docxWithMarker("FAKE:DELAY_MS=3000")

	var errA error
	var elapsedA time.Duration
	var wg sync.WaitGroup
	wg.Add(1)
	start := time.Now()
	go func() {
		defer wg.Done()
		res, err := f.ConvertToPDF(ctx, "a.docx", bytes.NewReader(held), convert.Options{})
		if err == nil {
			_ = res.Body.Close()
		}
		errA, elapsedA = err, time.Since(start)
	}()
	time.Sleep(300 * time.Millisecond)

	errs := make([]error, 2)
	var bc sync.WaitGroup
	for i := range errs {
		bc.Add(1)
		go func() {
			defer bc.Done()
			_, errs[i] = f.ConvertToPDF(ctx, "bc.docx", bytes.NewReader(held), convert.Options{})
		}()
	}
	bc.Wait()
	for i, err := range errs {
		wantErr(t, err, convert.ErrBusy)
		if !convert.Retryable(err) {
			t.Errorf("request %c: ErrBusy is not retryable", 'B'+i)
		}
	}

	wg.Wait()
	if errA != nil {
		t.Fatalf("request A: %v, want success", errA)
	}
	if elapsedA < 2500*time.Millisecond {
		t.Errorf("request A answered after %s, want about 3 s", elapsedA)
	}
}
