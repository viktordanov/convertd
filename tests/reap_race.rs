//! The reaper's stray pass and a worker that exits after its own peek
//!. A file of its own, because the stray
//! pass waits on every child of the test process.
#![cfg(all(target_os = "linux", feature = "test-hooks"))]

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use convertd::engines::process::{ReaperMode, Spawner};

/// Whether `pid` is a live process: it exists and is not a zombie.
fn alive(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next());
    state != Some("Z")
}

fn zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next().map(|s| s == "Z"))
        })
        .unwrap_or(false)
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// The worker's exit lands after its peek, so only the stray pass sees
/// it. That pass must still kill the worker's group: here a `sleep` that
/// the worker left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_reaped_as_a_stray_still_has_its_group_killed() {
    // No reaper task: the test decides which pass runs.
    let spawner = Spawner::without_reaper_task(ReaperMode::ProcessWide);
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!; exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let spawned = spawner.spawn(cmd).await.unwrap();
    let worker = spawned.child.pid() as i32;
    let stdout = std::fs::File::from(spawned.stdout.unwrap());
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .unwrap();
    let grandchild: i32 = line.trim().parse().unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || zombie(worker)),
        "the worker did not exit"
    );
    assert!(alive(grandchild), "the grandchild must outlive the worker");
    spawner.reap_strays_now();
    let killed = wait_until(Duration::from_secs(1), || !alive(grandchild));
    if !killed && let Some(pid) = rustix::process::Pid::from_raw(grandchild) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    assert!(killed, "the grandchild {grandchild} still runs");
    assert!(
        spawned.child.has_exited(),
        "the worker is reaped and finished"
    );
}
