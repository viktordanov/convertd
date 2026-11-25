//! `convertd engine …`: the engines without HTTP, for debugging and benchmarks (lane 002).

use std::ffi::OsString;
use std::process::ExitCode;

/// The hidden `engine` subcommands (INTERFACES.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommand {
    Convert,
    Inspect,
    Render,
    Assemble,
    Decrypt,
    Bench,
    Soak,
}

impl EngineCommand {
    /// The name used on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineCommand::Convert => "convert",
            EngineCommand::Inspect => "inspect",
            EngineCommand::Render => "render",
            EngineCommand::Assemble => "assemble",
            EngineCommand::Decrypt => "decrypt",
            EngineCommand::Bench => "bench",
            EngineCommand::Soak => "soak",
        }
    }
}

/// Runs one engine command. `args` are the arguments after the command
/// name, unparsed, so this lane defines its own flags without touching
/// `main.rs`.
///
/// Only `bench` exists in wave one; the other commands need the engines
/// (plan 002 steps 7, 8 and 13).
pub fn run(command: EngineCommand, args: Vec<OsString>) -> ExitCode {
    match command {
        EngineCommand::Bench => bench::run(args),
        other => {
            eprintln!("convertd engine {}: not implemented yet", other.as_str());
            ExitCode::from(2)
        }
    }
}

/// `convertd engine bench`: the supervisor's own cost per call and per
/// worker start, with no engine time (plan 002 step 6).
pub mod bench {
    use std::ffi::OsString;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;

    /// The flags of `convertd engine bench`.
    #[derive(Debug, Parser)]
    #[command(name = "convertd engine bench")]
    pub struct BenchArgs {
        /// The worker role: `test` (needs feature `test-hooks`); `office`
        /// and `render` come with the engines.
        #[arg(long, default_value = "test")]
        pub role: String,
        /// Measured lease + ping + return round trips.
        #[arg(long, default_value_t = 10_000)]
        pub n: usize,
        /// Concurrent callers; the pool has this many workers.
        #[arg(long, default_value_t = 1)]
        pub concurrency: usize,
        /// Measured worker starts (spawn to `Ready`).
        #[arg(long, default_value_t = 50)]
        pub spawns: usize,
        /// Untimed round trips before measuring.
        #[arg(long, default_value_t = 500)]
        pub warmup: usize,
    }

    /// Percentiles of a sample, in microseconds.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Summary {
        pub n: usize,
        pub p50: f64,
        pub p95: f64,
        pub p99: f64,
        pub max: f64,
        pub mean: f64,
    }

    /// Nearest-rank percentiles over `samples` (any order).
    pub fn summarize(samples: &mut [Duration]) -> Summary {
        samples.sort_unstable();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let rank = |p: f64| {
            let n = samples.len();
            let i = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
            us(samples[i])
        };
        if samples.is_empty() {
            return Summary {
                n: 0,
                p50: 0.0,
                p95: 0.0,
                p99: 0.0,
                max: 0.0,
                mean: 0.0,
            };
        }
        let total: Duration = samples.iter().sum();
        Summary {
            n: samples.len(),
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: us(samples[samples.len() - 1]),
            mean: us(total) / samples.len() as f64,
        }
    }

    impl Summary {
        pub fn json(&self) -> String {
            format!(
                "{{\"n\":{},\"p50\":{:.1},\"p95\":{:.1},\"p99\":{:.1},\"max\":{:.1},\"mean\":{:.1}}}",
                self.n, self.p50, self.p95, self.p99, self.max, self.mean
            )
        }

        pub fn row(&self, name: &str) -> String {
            format!(
                "{name:<22} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>9.1}",
                self.n, self.p50, self.p95, self.p99, self.max
            )
        }
    }

    pub fn run(args: Vec<OsString>) -> ExitCode {
        let argv = std::iter::once(OsString::from("convertd engine bench")).chain(args);
        let args = match BenchArgs::try_parse_from(argv) {
            Ok(args) => args,
            Err(err) => {
                let _ = err.print();
                return ExitCode::from(2);
            }
        };
        match args.role.as_str() {
            "test" => test_role(&args),
            "office" | "render" => {
                eprintln!(
                    "convertd engine bench: --role {} needs the engines (plan 002 steps 7 and 8)",
                    args.role
                );
                ExitCode::from(2)
            }
            other => {
                eprintln!("convertd engine bench: unknown role {other:?}");
                ExitCode::from(2)
            }
        }
    }

    #[cfg(not(feature = "test-hooks"))]
    fn test_role(_args: &BenchArgs) -> ExitCode {
        eprintln!("convertd engine bench: --role test needs a build with --features test-hooks");
        ExitCode::from(2)
    }

    #[cfg(feature = "test-hooks")]
    fn test_role(args: &BenchArgs) -> ExitCode {
        let runtime = match crate::runtime::serve(None) {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("convertd engine bench: failed to build the runtime: {err}");
                return ExitCode::FAILURE;
            }
        };
        match runtime.block_on(measure(args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("convertd engine bench: {err}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(feature = "test-hooks")]
    async fn measure(args: &BenchArgs) -> Result<(), String> {
        use std::time::Instant;

        use crate::engines::config::Paper;
        use crate::engines::pool::{Admission, Pool, PoolSpec, start_once};
        use crate::engines::process::{ReaperMode, Spawner};
        use crate::engines::slot::WorkerEnv;
        use crate::ipc::{Init, Op, PROTOCOL_VERSION};
        use crate::worker::Role;

        let concurrency = args.concurrency.max(1);
        let base = std::env::temp_dir().join(format!("convertd-bench-{}", std::process::id()));
        let program =
            std::env::current_exe().map_err(|err| format!("failed to find the binary: {err}"))?;
        let spec = PoolSpec {
            name: "test",
            role: Role::Test,
            program,
            workers_dir: base.join("workers"),
            profile_template: None,
            env: WorkerEnv::from_process(Paper::A4),
            init: Init {
                protocol: PROTOCOL_VERSION,
                ..Init::default()
            },
            size: u16::try_from(concurrency).map_err(|_| "too many workers".to_string())?,
            max_jobs: u32::MAX,
            max_rss: u64::MAX,
            kill_rss: u64::MAX,
            max_age: Duration::from_secs(86_400),
            age_check: Duration::from_secs(30),
            queue_capacity: concurrency * 2,
            queue_wait: Duration::from_secs(10),
            start_timeout: Duration::from_secs(60),
        };
        let spawner = Spawner::new(ReaperMode::Owned)
            .map_err(|err| format!("failed to start the spawner: {err}"))?;

        // Spawn to Ready, one at a time, on the pool's own start path.
        let mut prepare = Vec::with_capacity(args.spawns);
        let mut spawn = Vec::with_capacity(args.spawns);
        let mut bind = Vec::with_capacity(args.spawns);
        let mut rss = Vec::with_capacity(args.spawns);
        for i in 0..args.spawns {
            let slot = u16::try_from(1000 + i % 8).unwrap_or(1000);
            let times = start_once(&spec, &spawner, slot).await?;
            prepare.push(times.prepare);
            spawn.push(times.spawn);
            bind.push(times.bind);
            rss.push(times.rss_bytes);
        }

        let pool = Pool::start(spec, spawner);
        if !pool.wait_idle(concurrency, Duration::from_secs(60)).await {
            pool.shutdown(Duration::from_secs(1)).await;
            return Err(format!("the pool did not get ready: {:?}", pool.status()));
        }
        let round = |pool: Pool, count: usize| async move {
            let mut total = Vec::with_capacity(count);
            let mut lease_only = Vec::with_capacity(count);
            let mut call_only = Vec::with_capacity(count);
            for _ in 0..count {
                let t0 = Instant::now();
                let deadline = t0 + Duration::from_secs(10);
                let mut lease = pool
                    .lease(deadline, Admission::Queue)
                    .await
                    .map_err(|err| err.to_string())?;
                let t1 = Instant::now();
                lease
                    .call(Op::Ping, deadline)
                    .await
                    .map_err(|err| err.to_string())?
                    .result
                    .map_err(|err| err.to_string())?;
                let t2 = Instant::now();
                drop(lease);
                total.push(t0.elapsed());
                lease_only.push(t1 - t0);
                call_only.push(t2 - t1);
            }
            Ok::<_, String>((total, lease_only, call_only))
        };
        let per_task = |n: usize| n.div_ceil(concurrency);
        let run_all = |n: usize| {
            let tasks: Vec<_> = (0..concurrency)
                .map(|_| tokio::spawn(round(pool.clone(), per_task(n))))
                .collect();
            async move {
                let mut all = (Vec::new(), Vec::new(), Vec::new());
                for task in tasks {
                    let (t, l, c) = task.await.map_err(|err| err.to_string())??;
                    all.0.extend(t);
                    all.1.extend(l);
                    all.2.extend(c);
                }
                Ok::<_, String>(all)
            }
        };
        run_all(args.warmup).await?;
        let wall = Instant::now();
        let (mut total, mut lease_only, mut call_only) = run_all(args.n).await?;
        let wall = wall.elapsed();
        let pool_status = pool.status();
        pool.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(&base);

        let total = summarize(&mut total);
        let lease_only = summarize(&mut lease_only);
        let call_only = summarize(&mut call_only);
        let prepare = summarize(&mut prepare);
        let spawn = summarize(&mut spawn);
        let bind = summarize(&mut bind);
        rss.sort_unstable();
        let rss_p50 = rss.get(rss.len() / 2).copied().unwrap_or(0);

        eprintln!(
            "convertd engine bench --role test: n={} concurrency={} spawns={} ({:.0} calls/s, {:?})",
            total.n,
            concurrency,
            spawn.n,
            total.n as f64 / wall.as_secs_f64(),
            pool_status
        );
        eprintln!(
            "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
            "microseconds", "n", "p50", "p95", "p99", "max"
        );
        eprintln!("{}", total.row("lease+ping+return"));
        eprintln!("{}", lease_only.row("  lease"));
        eprintln!("{}", call_only.row("  ping call"));
        eprintln!("{}", spawn.row("spawn to ready"));
        eprintln!("{}", bind.row("  bind (in worker)"));
        eprintln!("{}", prepare.row("slot prepare"));
        eprintln!("worker rss at ready, p50: {rss_p50} bytes");
        println!(
            "{{\"op\":\"bench\",\"role\":\"test\",\"concurrency\":{concurrency},\"lease_ping_us\":{},\"lease_us\":{},\"call_us\":{},\"spawn_us\":{},\"bind_us\":{},\"prepare_us\":{},\"rss_bytes_p50\":{rss_p50}}}",
            total.json(),
            lease_only.json(),
            call_only.json(),
            spawn.json(),
            bind.json(),
            prepare.json()
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nearest_rank_percentiles() {
            let mut samples: Vec<Duration> = (1..=100).map(Duration::from_micros).collect();
            let s = summarize(&mut samples);
            assert_eq!((s.p50, s.p95, s.p99, s.max), (50.0, 95.0, 99.0, 100.0));
            assert!((s.mean - 50.5).abs() < 1e-9);
            assert_eq!(summarize(&mut []).n, 0);
        }

        #[test]
        fn flags_parse() {
            let args = BenchArgs::try_parse_from([
                "bench",
                "--role",
                "test",
                "--n",
                "10",
                "--concurrency",
                "2",
            ])
            .unwrap();
            assert_eq!((args.n, args.concurrency, args.spawns), (10, 2, 50));
        }
    }
}
