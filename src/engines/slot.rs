//! Worker slot directories and the worker environment.
//!
//! Each worker process gets `<workers_dir>/<pool>-<slot>/` with `profile/`,
//! `tmp/`, `cache/`, `config/`, `data/` and `run/`, recreated for every
//! new process, so nothing one worker plants is loaded by the next.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::engines::config::Paper;
use crate::worker::sys::{ENV_SUPERVISOR_PID, ENV_WORKER_ID};

/// The subdirectories of a slot.
pub const SLOT_SUBDIRS: [&str; 6] = ["profile", "tmp", "cache", "config", "data", "run"];

/// The `PATH` a worker gets.
pub const WORKER_PATH: &str = "/usr/bin:/bin";

/// One slot directory.
#[derive(Debug, Clone)]
pub struct SlotDir {
    root: PathBuf,
}

impl SlotDir {
    pub fn new(workers_dir: &Path, pool: &str, slot: u16) -> Self {
        Self {
            root: workers_dir.join(format!("{pool}-{slot}")),
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Removes the slot and creates it again, empty, mode 0700, with the
    /// profile template copied into `profile/` if one is set. Blocking.
    pub fn prepare(&self, profile_template: Option<&Path>) -> io::Result<()> {
        self.remove()?;
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&self.root)?;
        for sub in SLOT_SUBDIRS {
            builder.create(self.root.join(sub))?;
        }
        if let Some(template) = profile_template {
            copy_tree(template, &self.root.join("profile"))?;
        }
        Ok(())
    }

    /// Removes the slot. Blocking; a missing slot is fine.
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            DirBuilder::new().mode(0o700).create(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        }
        // Symlinks and special files in a template are skipped.
    }
    Ok(())
}

/// The environment of a worker: built after `env_clear()` from a fixed
/// allow-list, so no service secret (`API_KEY`) or configuration reaches
/// a worker.
#[derive(Debug, Clone)]
pub struct WorkerEnv {
    /// Variables passed through from the supervisor's environment.
    passthrough: Vec<(OsString, OsString)>,
    lc_paper: &'static str,
    /// A directory put in front of [`WORKER_PATH`].
    path_prefix: Option<PathBuf>,
    /// `LC_ALL` was set and is not passed on (it would override `LC_PAPER`).
    pub lc_all_dropped: bool,
    /// `LANG` (other than `C.UTF-8`) and the `LC_*` variables other than
    /// `LC_ALL` and `LC_PAPER` that were set and are not passed on.
    pub locale_dropped: Vec<String>,
}

/// The locale of every worker. LibreOffice formats numbers and dates with
/// the system locale, because the image keeps `ooSetupSystemLocale` empty
/// (docker/libreoffice/convertd-paper.xcd) so that `LC_PAPER` decides the
/// default paper. A fixed `LANG` keeps that formatting en-US, as it was when
/// LibreOfficeKit forced en-US, whatever the deployment sets.
const WORKER_LANG: &str = "C.UTF-8";

const PASSTHROUGH: [&str; 5] = [
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
    "FONTCONFIG_SYSROOT",
    "TZ",
    "LD_LIBRARY_PATH",
];

impl WorkerEnv {
    /// From the supervisor's own environment.
    pub fn from_process(paper: Paper) -> Self {
        Self::from_vars(std::env::vars_os(), paper)
    }

    pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>, paper: Paper) -> Self {
        let mut passthrough = Vec::new();
        let mut lc_all_dropped = false;
        let mut locale_dropped = Vec::new();
        for (name, value) in vars {
            let Some(key) = name.to_str() else { continue };
            if key == "LANG" {
                if value != WORKER_LANG {
                    locale_dropped.push(key.to_owned());
                }
            } else if key == "LC_ALL" {
                lc_all_dropped = true;
            } else if key.starts_with("LC_") {
                if key != "LC_PAPER" {
                    locale_dropped.push(key.to_owned());
                }
            } else if PASSTHROUGH.contains(&key) {
                passthrough.push((name, value));
            }
        }
        passthrough.push(("LANG".into(), WORKER_LANG.into()));
        passthrough.sort();
        locale_dropped.sort();
        Self {
            passthrough,
            lc_paper: paper.lc_paper(),
            path_prefix: None,
            lc_all_dropped,
            locale_dropped,
        }
    }

    /// Puts `dir` in front of `PATH`. For the LibreOffice program
    /// directory: the libreofficekit crate prepends it with a `;` (a
    /// Windows separator) unless `PATH` already holds it, which would cut
    /// `/usr/bin` off the worker's `PATH`.
    pub fn with_path_prefix(mut self, dir: &Path) -> Self {
        self.path_prefix = Some(dir.to_path_buf());
        self
    }

    /// Every variable a worker in `slot` gets, sorted by name.
    pub fn vars(&self, slot: &SlotDir, worker_id: &str) -> Vec<(OsString, OsString)> {
        let root = slot.path();
        let sub = |name: &str| root.join(name).into_os_string();
        let mut profile = OsString::from("file://");
        profile.push(root.join("profile"));
        let mut vars = self.passthrough.clone();
        vars.extend([
            ("UserInstallation".into(), profile),
            ("HOME".into(), root.as_os_str().to_owned()),
            ("TMPDIR".into(), sub("tmp")),
            ("XDG_CACHE_HOME".into(), sub("cache")),
            ("XDG_CONFIG_HOME".into(), sub("config")),
            ("XDG_DATA_HOME".into(), sub("data")),
            ("XDG_RUNTIME_DIR".into(), sub("run")),
            ("LC_PAPER".into(), self.lc_paper.into()),
            ("PATH".into(), self.path()),
            (
                ENV_SUPERVISOR_PID.into(),
                std::process::id().to_string().into(),
            ),
            (ENV_WORKER_ID.into(), worker_id.into()),
        ]);
        vars.sort();
        vars
    }

    fn path(&self) -> OsString {
        match &self.path_prefix {
            Some(dir) => {
                let mut path = dir.as_os_str().to_owned();
                path.push(":");
                path.push(WORKER_PATH);
                path
            }
            None => WORKER_PATH.into(),
        }
    }

    /// `program worker <role>` in `slot`, with this environment and all
    /// three stdio streams piped.
    pub fn command(&self, program: &Path, role: &str, slot: &SlotDir, worker_id: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(["worker", role])
            .env_clear()
            .envs(self.vars(slot, worker_id))
            .current_dir(slot.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(vars: &[(OsString, OsString)]) -> Vec<String> {
        vars.iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_the_allow_list_passes() {
        let env = WorkerEnv::from_vars(
            [
                ("API_KEY", "secret"),
                ("API_KEY_PREVIOUS", "old"),
                ("LC_ALL", "C"),
                ("LC_PAPER", "de_DE.UTF-8"),
                ("LC_TIME", "en_GB.UTF-8"),
                ("TZ", "Europe/Berlin"),
                ("FONTCONFIG_FILE", "/etc/fonts/x.conf"),
                ("PORT", "8080"),
                ("PATH", "/usr/local/bin"),
            ]
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
            Paper::Letter,
        );
        assert!(env.lc_all_dropped);
        assert_eq!(env.locale_dropped, ["LC_TIME"]);
        let slot = SlotDir::new(Path::new("/tmp/convert/workers"), "office", 0);
        let vars = env.vars(&slot, "office-0");
        assert_eq!(
            names(&vars),
            [
                "CONVERTD_SUPERVISOR_PID",
                "CONVERTD_WORKER_ID",
                "FONTCONFIG_FILE",
                "HOME",
                "LANG",
                "LC_PAPER",
                "PATH",
                "TMPDIR",
                "TZ",
                "UserInstallation",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_RUNTIME_DIR",
            ]
        );
        let get = |name: &str| {
            vars.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string_lossy().into_owned())
                .unwrap()
        };
        assert_eq!(get("LC_PAPER"), "en_US.UTF-8");
        assert_eq!(get("LANG"), "C.UTF-8");
        assert_eq!(get("PATH"), WORKER_PATH);
        assert_eq!(get("HOME"), "/tmp/convert/workers/office-0");
        assert_eq!(
            get("UserInstallation"),
            "file:///tmp/convert/workers/office-0/profile"
        );
    }

    #[test]
    fn the_worker_locale_is_fixed_whatever_the_supervisor_sets() {
        let lang = |vars: &[(&str, &str)]| {
            let env = WorkerEnv::from_vars(
                vars.iter()
                    .map(|(k, v)| (OsString::from(k), OsString::from(v))),
                Paper::A4,
            );
            let slot = SlotDir::new(Path::new("/tmp/w"), "office", 0);
            let value = env
                .vars(&slot, "office-0")
                .into_iter()
                .find(|(k, _)| k == "LANG")
                .map(|(_, v)| v.to_string_lossy().into_owned())
                .unwrap();
            (value, env.locale_dropped)
        };
        assert_eq!(lang(&[]), ("C.UTF-8".to_owned(), Vec::<String>::new()));
        assert_eq!(
            lang(&[("LANG", "C.UTF-8")]),
            ("C.UTF-8".to_owned(), Vec::<String>::new())
        );
        assert_eq!(
            lang(&[("LANG", "en_GB.UTF-8"), ("LC_NUMERIC", "de_DE.UTF-8")]),
            (
                "C.UTF-8".to_owned(),
                vec!["LANG".to_owned(), "LC_NUMERIC".to_owned()]
            )
        );
    }

    #[test]
    fn the_program_dir_goes_in_front_of_path() {
        let env = WorkerEnv::from_vars(Vec::new(), Paper::A4)
            .with_path_prefix(Path::new("/opt/libreoffice/program"));
        let slot = SlotDir::new(Path::new("/tmp/w"), "office", 0);
        let path = env
            .vars(&slot, "office-0")
            .into_iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(path, "/opt/libreoffice/program:/usr/bin:/bin");
    }

    #[test]
    fn prepare_recreates_the_slot_and_copies_the_template() {
        let base = std::env::temp_dir().join(format!("convertd-002-slot-{}", std::process::id()));
        let template = base.join("template");
        fs::create_dir_all(template.join("user")).unwrap();
        fs::write(template.join("user/registrymodifications.xcu"), "x").unwrap();
        let slot = SlotDir::new(&base.join("workers"), "office", 3);
        slot.prepare(None).unwrap();
        fs::write(slot.path().join("tmp/planted"), "x").unwrap();
        slot.prepare(Some(&template)).unwrap();
        assert!(!slot.path().join("tmp/planted").exists());
        for sub in SLOT_SUBDIRS {
            assert!(slot.path().join(sub).is_dir(), "{sub}");
        }
        assert!(
            slot.path()
                .join("profile/user/registrymodifications.xcu")
                .is_file()
        );
        slot.remove().unwrap();
        assert!(!slot.path().exists());
        fs::remove_dir_all(base).unwrap();
    }
}
