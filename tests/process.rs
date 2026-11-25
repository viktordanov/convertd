//! Process control: spawning, scoped reaping, group kills (plan 002 step 3).
#![cfg(feature = "test-hooks")]

use std::collections::HashSet;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use convertd::engines::config::Paper;
use convertd::engines::conn::Conn;
use convertd::engines::process::{ChildHandle, ReaperMode, Spawner, drain_stderr};
use convertd::engines::slot::{SlotDir, WorkerEnv};
use convertd::ipc::*;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};

const HELPER_VAR: &str = "CONVERTD_002_HELPER";

fn program() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_convertd"))
}

fn workers_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("convertd-002-{name}-{}", std::process::id()))
}

struct Started {
    child: ChildHandle,
    conn: Conn,
    ready: Ready,
}

fn env() -> WorkerEnv {
    WorkerEnv::from_vars(Vec::new(), Paper::A4)
}

/// Spawns `worker test` in a fresh slot and waits for `Ready`.
async fn start(spawner: &Spawner, dir: &Path, slot: u16) -> Started {
    start_with(spawner, &env(), dir, slot).await
}

async fn start_with(spawner: &Spawner, env: &WorkerEnv, dir: &Path, slot: u16) -> Started {
    let (child, mut conn) = spawn_only(spawner, env, dir, slot).await;
    conn.send(&Request::Init(Init {
        protocol: PROTOCOL_VERSION,
        ..Init::default()
    }))
    .await
    .unwrap();
    let ready = match conn.recv().await.unwrap() {
        Some(Response::Ready(ready)) => ready,
        other => panic!("expected ready, got {other:?}"),
    };
    Started { child, conn, ready }
}

async fn spawn_only(
    spawner: &Spawner,
    env: &WorkerEnv,
    dir: &Path,
    slot: u16,
) -> (ChildHandle, Conn) {
    let slot_dir = SlotDir::new(dir, "test", slot);
    slot_dir.prepare(None).unwrap();
    let id = format!("test-{slot}");
    let cmd = env.command(program(), "test", &slot_dir, &id);
    let spawned = spawner.spawn(cmd).await.unwrap();
    drain_stderr(spawned.stderr.unwrap(), id).unwrap();
    let conn = Conn::new(spawned.stdin.unwrap(), spawned.stdout.unwrap()).unwrap();
    (spawned.child, conn)
}

async fn call(conn: &mut Conn, op: Op) -> Result<Output, WorkerError> {
    conn.send(&Request::Call(Call { id: 1, op })).await.unwrap();
    match conn.recv().await.unwrap() {
        Some(Response::Reply(reply)) => reply.result,
        other => panic!("expected a reply, got {other:?}"),
    }
}

/// The process no longer runs: it does not exist, or (Linux) it is a
/// zombie waiting for a parent that is not us, such as a PID 1 that does
/// not reap orphans in a container.
fn gone(pid: u32) -> bool {
    let raw = Pid::from_raw(pid as i32).unwrap();
    if test_kill_process(raw) == Err(Errno::SRCH) {
        return true;
    }
    zombie(pid)
}

#[cfg(target_os = "linux")]
fn zombie(pid: u32) -> bool {
    // /proc/<pid>/stat: "pid (comm) S ...", where comm may hold spaces.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn zombie(_pid: u32) -> bool {
    false
}

async fn wait_gone(pid: u32, within: Duration) -> bool {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gone(pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_gives_the_exit_status_from_the_reaper() {
    let dir = workers_dir("kill");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let mut w = start(&spawner, &dir, 0).await;
    assert_eq!(w.ready.pid, w.child.pid());
    assert_eq!(call(&mut w.conn, Op::Ping).await, Ok(Output::Pong));
    assert!(w.child.kill_group());
    let exit = tokio::time::timeout(Duration::from_secs(5), w.child.wait())
        .await
        .expect("the reaper reports the exit");
    assert_eq!(exit.status.unwrap().signal(), Some(libc::SIGKILL));
    assert!(w.child.has_exited());
    // After the reap, kill_group sends nothing: the PID may be reused.
    assert!(!w.child.kill_group());
    assert_eq!(spawner.live_children(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_kill_takes_the_workers_children() {
    let dir = workers_dir("sleeper");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let mut w = start(&spawner, &dir, 0).await;
    let Ok(Output::Test(TestOutput::Spawned { pid })) =
        call(&mut w.conn, Op::Test(TestOp::SpawnSleeper)).await
    else {
        panic!("expected spawned");
    };
    assert!(!gone(pid), "the sleeper runs");
    w.child.kill_group();
    w.child.wait().await;
    assert!(
        wait_gone(pid, Duration::from_secs(3)).await,
        "the sleeper {pid} outlived its group"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// PIDs of our children in state Z, from `ps`, run through the spawner.
async fn zombies_among(spawner: &Spawner, pids: &HashSet<u32>) -> Vec<String> {
    let mut ps = Command::new("ps");
    ps.args(["-A", "-o", "pid=,ppid=,stat="]);
    let done = spawner
        .run_to_completion(ps, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(done.status.and_then(|s| s.code()), Some(0));
    let me = std::process::id();
    String::from_utf8_lossy(&done.stdout)
        .lines()
        .filter(|line| {
            let mut f = line.split_whitespace();
            let pid: u32 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let ppid: u32 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let stat = f.next().unwrap_or("");
            ppid == me && stat.starts_with('Z') && pids.contains(&pid)
        })
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_hundred_spawn_kill_cycles_leave_no_zombies() {
    let dir = workers_dir("cycles");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let mut pids = HashSet::new();
    for i in 0..200 {
        let w = start(&spawner, &dir, (i % 4) as u16).await;
        pids.insert(w.child.pid());
        w.child.kill_group();
        let exit = tokio::time::timeout(Duration::from_secs(5), w.child.wait())
            .await
            .expect("reaped");
        assert!(exit.status.is_some());
    }
    assert_eq!(pids.len(), 200);
    assert_eq!(spawner.live_children(), 0);
    let zombies = zombies_among(&spawner, &pids).await;
    assert!(zombies.is_empty(), "zombies: {zombies:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_spawners_each_see_only_their_own_exits() {
    let dir = workers_dir("scoped");
    let a = Spawner::new(ReaperMode::Owned).unwrap();
    let b = Spawner::new(ReaperMode::Owned).unwrap();
    let run = |spawner: Spawner, base: u16| {
        let dir = dir.clone();
        async move {
            let mut seen = 0;
            for i in 0..50 {
                let (child, _conn) = spawn_only(&spawner, &env(), &dir, base + (i % 4)).await;
                child.kill_group();
                let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .expect("reaped");
                assert_eq!(
                    exit.status.and_then(|s| s.signal()),
                    Some(libc::SIGKILL),
                    "a status was lost or stolen"
                );
                seen += 1;
            }
            assert_eq!(spawner.live_children(), 0);
            seen
        }
    };
    let (sa, sb) = tokio::join!(tokio::spawn(run(a, 0)), tokio::spawn(run(b, 10)));
    assert_eq!((sa.unwrap(), sb.unwrap()), (50, 50));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::disallowed_methods)] // plays a dependency that waits on its own child
async fn a_dependency_waiting_on_its_own_child_is_not_disturbed() {
    let dir = workers_dir("foreign");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    // Keep the reaper busy while std waits on its own children.
    let churn = {
        let spawner = spawner.clone();
        let dir = dir.clone();
        tokio::spawn(async move {
            for i in 0..20 {
                let (child, _conn) = spawn_only(&spawner, &env(), &dir, i % 2).await;
                child.kill_group();
                child.wait().await;
            }
        })
    };
    let waits = tokio::task::spawn_blocking(|| {
        (0..20)
            .map(|_| Command::new("true").status().map(|s| s.success()))
            .collect::<Vec<_>>()
    });
    for status in waits.await.unwrap() {
        assert!(status.unwrap(), "std lost its own child's status");
    }
    churn.await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_environment_is_exactly_the_allow_list() {
    let dir = workers_dir("env");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let env = WorkerEnv::from_vars(
        [
            ("API_KEY", "secret"),
            ("API_KEY_PREVIOUS", "old"),
            ("LC_ALL", "C"),
            ("TZ", "UTC"),
            ("PORT", "8080"),
        ]
        .map(|(k, v)| (k.into(), v.into())),
        Paper::A4,
    );
    let mut w = start_with(&spawner, &env, &dir, 0).await;
    let Ok(Output::Test(TestOutput::Env { vars })) = call(&mut w.conn, Op::Test(TestOp::Env)).await
    else {
        panic!("expected env");
    };
    let expected: Vec<(String, String)> = env
        .vars(&SlotDir::new(&dir, "test", 0), "test-0")
        .into_iter()
        .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
        .collect();
    assert_eq!(vars, expected);
    assert!(vars.iter().all(|(k, _)| !k.starts_with("API_KEY")));
    w.child.kill_group();
    w.child.wait().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_to_completion_captures_and_times_out() {
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let mut echo = Command::new("sh");
    echo.args(["-c", "echo out; echo err >&2; exit 3"]);
    let done = spawner
        .run_to_completion(echo, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(done.status.and_then(|s| s.code()), Some(3));
    assert_eq!(done.stdout, b"out\n");
    assert_eq!(done.stderr, b"err\n");

    let mut slow = Command::new("sleep");
    slow.arg("30");
    let started = Instant::now();
    let err = spawner
        .run_to_completion(slow, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(spawner.live_children(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_spawner_kills_and_reaps_its_children() {
    let dir = workers_dir("drop");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let w = start(&spawner, &dir, 0).await;
    let pid = w.child.pid();
    drop(spawner);
    // The handle holds only a weak reference: the spawner is gone.
    assert!(!w.child.kill_group());
    assert!(w.child.has_exited());
    assert!(wait_gone(pid, Duration::from_secs(2)).await);
    let _ = std::fs::remove_dir_all(dir);
}

/// Runs one `helper_*` test of this binary in a child process.
#[allow(clippy::disallowed_methods)] // the test reaps its own helper process
fn run_helper(name: &str) -> std::process::ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(HELPER_VAR, name)
        .status()
        .unwrap()
}

fn is_helper(name: &str) -> bool {
    std::env::var(HELPER_VAR).as_deref() == Ok(name)
}

#[test]
fn process_wide_mode_reaps_strays_and_keeps_registered_statuses() {
    // In its own process: the P_ALL sweep would steal other tests' children.
    assert!(run_helper("helper_process_wide").success());
}

#[test]
fn helper_process_wide() {
    if !is_helper("helper_process_wide") {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = workers_dir("process-wide");
        let spawner = Spawner::new(ReaperMode::ProcessWide).unwrap();
        // A stray: a child nobody registered and nobody waits for.
        let stray = Command::new("true").spawn().unwrap();
        let stray_pid = stray.id();
        drop(stray);
        assert!(
            wait_gone(stray_pid, Duration::from_secs(5)).await,
            "the stray was not reaped"
        );
        // A registered worker still gets its own status.
        let w = start(&spawner, &dir, 0).await;
        w.child.kill_group();
        let exit = w.child.wait().await;
        assert_eq!(exit.status.and_then(|s| s.signal()), Some(libc::SIGKILL));
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_outlives_the_thread_that_asked_for_it() {
    let dir = workers_dir("thread");
    let spawner = Spawner::new(ReaperMode::Owned).unwrap();
    let handle = tokio::runtime::Handle::current();
    let (s, d) = (spawner.clone(), dir.clone());
    let mut w = std::thread::spawn(move || handle.block_on(start(&s, &d, 0)))
        .join()
        .unwrap();
    // The asking thread is gone; the parent-death signal is tied to the
    // spawner thread, which lives on.
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(call(&mut w.conn, Op::Ping).await, Ok(Output::Pong));
    w.child.kill_group();
    w.child.wait().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::disallowed_methods)] // the test reaps its own helper process
fn a_hung_worker_dies_with_a_killed_supervisor() {
    use std::io::BufRead;

    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_supervisor",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_VAR, "helper_supervisor")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(helper.stdout.take().unwrap()).lines();
    let pid: u32 = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|l| l.strip_prefix("WORKER ").and_then(|p| p.parse().ok()))
        .expect("the helper reports its worker");
    assert!(!gone(pid));
    helper.kill().unwrap();
    helper.wait().unwrap();
    let until = Instant::now() + Duration::from_secs(1);
    while !gone(pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone(pid), "the hung worker {pid} outlived its supervisor");
}

#[cfg(target_os = "linux")]
#[test]
fn helper_supervisor() {
    if !is_helper("helper_supervisor") {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = workers_dir("supervisor");
        let spawner = Spawner::new(ReaperMode::Owned).unwrap();
        let mut w = start(&spawner, &dir, 0).await;
        w.conn
            .send(&Request::Call(Call {
                id: 1,
                op: Op::Test(TestOp::Hang),
            }))
            .await
            .unwrap();
        println!("WORKER {}", w.child.pid());
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
}
