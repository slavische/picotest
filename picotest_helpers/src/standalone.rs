//! Standalone mode: tests run inside a single bare Picodata instance.
//!
//! Unlike the plugin modes, this one needs neither `topology.toml` nor
//! pike. The crate under test is built as a cdylib and every test calls its
//! own symbol from it through the admin console of the instance.
//!
//! The instance is started lazily on the first test and is shared by all
//! tests of the test binary. Tests run one after another (see [`acquire`]),
//! since they share global state of the instance.

use crate::slot::ClusterSlot;
use crate::{
    cargo_target_dir, instance_stop_signal, lock_new_data_dir, remove_stale_data_dirs,
    run_admin_lua, use_release_build, ADMIN_SOCKET_NAME,
};
use anyhow::{bail, Context};
use log::{debug, warn};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
const LIB_EXT: &str = "so";

#[cfg(target_os = "macos")]
const LIB_EXT: &str = "dylib";

const ENV_PICODATA_PATH: &str = "PICODATA_PATH";
const INSTANCE_DIR_NAME: &str = "i1";
const INSTANCE_LOG_FILE_NAME: &str = "picodata.log";
const LOG_PREFIX: &str = "i1: ";

/// How long to wait until the admin console of a fresh instance executes requests.
const ADMIN_SOCKET_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the instance to exit after a failed admin console request.
const CRASH_DETECTION_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the instance to exit after the stop signal before killing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

static STATE: Mutex<State> = Mutex::new(State::NotStarted);

enum State {
    NotStarted,
    Running(Instance),
    /// The instance failed to start. Kept to fail the remaining tests
    /// quickly instead of rebuilding the crate for each of them.
    FailedToStart(String),
}

/// Running Picodata instance shared by standalone tests of the test binary.
struct Instance {
    picodata_path: PathBuf,
    instance_dir: PathBuf,
    socket_path: PathBuf,
    child: Child,
    /// Name of the last test which found the instance alive.
    last_test: Option<String>,
    _slot: ClusterSlot,
    _data_dir_lock: File,
}

impl Instance {
    fn log_path(&self) -> PathBuf {
        self.instance_dir.join(INSTANCE_LOG_FILE_NAME)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Waits up to `timeout` for the instance to exit.
    fn is_alive_after(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.is_alive() {
            if start.elapsed() >= timeout {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

/// Constructs path to the cdylib of the crate under test.
///
/// ### Arguments
/// - `crate_name` - name of the lib target (`CARGO_CRATE_NAME`).
/// - `manifest_dir` - directory of the crate manifest (`CARGO_MANIFEST_DIR`).
///   Used to locate the target directory if the test binary is not run
///   from it.
pub fn dylib_path(crate_name: &str, manifest_dir: &str) -> PathBuf {
    target_dir(Path::new(manifest_dir))
        .join(profile())
        .join(format!("lib{}.{LIB_EXT}", crate_name.replace('-', "_")))
}

fn target_dir(manifest_dir: &Path) -> PathBuf {
    cargo_target_dir().unwrap_or_else(|| manifest_dir.join("target"))
}

fn profile() -> &'static str {
    if use_release_build() {
        "release"
    } else {
        "debug"
    }
}

/// Exclusive access to the standalone instance for the duration of one test.
///
/// Holding the guard keeps other standalone tests of the test binary waiting,
/// so they run one after another even if libtest runs tests in parallel.
pub struct StandaloneInstance {
    state: MutexGuard<'static, State>,
    test_name: String,
}

/// Acquires the standalone instance for the test `test_name`, starting it on first call.
///
/// `manifest_dir` is the directory of the crate under test: the crate is
/// built there and the instance data is kept in its `tmp/tests` subdirectory.
///
/// Panics if the instance failed to start.
pub fn acquire(test_name: &str, manifest_dir: &str) -> StandaloneInstance {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);

    if let State::NotStarted = *state {
        *state = match start(Path::new(manifest_dir)) {
            Ok(instance) => State::Running(instance),
            Err(err) => State::FailedToStart(format!("{err:#}")),
        };
    }

    if let State::FailedToStart(err) = &*state {
        panic!("Failed to start standalone picodata instance: {err}");
    }

    StandaloneInstance {
        state,
        test_name: test_name.to_string(),
    }
}

impl StandaloneInstance {
    /// Executes Lua script through the admin console of the instance.
    ///
    /// Panics with a pointer to the instance log if the instance is dead
    /// before the request or dies during it. The request is considered
    /// failed if it returns an error or its output has no `true`: the
    /// script generated for a test ends with it.
    pub fn run_lua<T: AsRef<[u8]>>(&mut self, query: T) -> io::Result<String> {
        let State::Running(instance) = &mut *self.state else {
            unreachable!("guard is only created for a running instance");
        };

        if !instance.is_alive() {
            match &instance.last_test {
                Some(test) => panic!(
                    "picodata instance crashed earlier, during test '{test}', see {}",
                    instance.log_path().display()
                ),
                None => panic!(
                    "picodata instance crashed before the first test, see {}",
                    instance.log_path().display()
                ),
            }
        }
        instance.last_test = Some(self.test_name.clone());

        let result = run_admin_lua(
            &instance.picodata_path,
            &instance.socket_path,
            query.as_ref(),
        );

        // A crashing instance drops the console connection, which isn't an
        // error for `picodata admin`: the output just ends early. The script
        // of a test ends with `true`, so its absence means the request failed.
        let finished = matches!(&result, Ok(output) if output.contains("true"));
        if !finished && !instance.is_alive_after(CRASH_DETECTION_TIMEOUT) {
            panic!(
                "picodata instance crashed during this test, see {}",
                instance.log_path().display()
            );
        }

        result
    }
}

/// Stops the standalone instance if it was started.
///
/// Called from the destructor of the test binary.
pub fn stop() -> anyhow::Result<()> {
    // A test thread may still hold the guard while the process is exiting,
    // don't wait for it.
    let mut state = match STATE.try_lock() {
        Ok(state) => state,
        Err(TryLockError::Poisoned(err)) => err.into_inner(),
        Err(TryLockError::WouldBlock) => bail!("standalone instance is still in use"),
    };

    let State::Running(instance) = &mut *state else {
        return Ok(());
    };

    if !instance.is_alive() {
        return Ok(());
    }

    let pid = Pid::from_raw(instance.child.id() as i32);
    let signal = instance_stop_signal();
    debug!("Stopping standalone picodata instance {pid} with {signal}");
    kill(pid, signal).context("failed to send stop signal to picodata instance")?;

    if instance.is_alive_after(STOP_TIMEOUT) {
        warn!("Picodata instance {pid} did not stop in {STOP_TIMEOUT:?}, killing it");
        kill(pid, Signal::SIGKILL).context("failed to kill picodata instance")?;
        instance.child.wait()?;
    }

    Ok(())
}

fn picodata_path() -> PathBuf {
    std::env::var_os(ENV_PICODATA_PATH)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            println!(
                "PICODATA_PATH environment variable is not set, \
                using default picodata binary from PATH"
            );
            PathBuf::from("picodata")
        })
}

fn start(manifest_dir: &Path) -> anyhow::Result<Instance> {
    build_crate(manifest_dir)?;

    let slot = ClusterSlot::acquire(1, &BTreeMap::new())?;

    let (data_dir, data_dir_lock) = lock_new_data_dir(manifest_dir)?;
    let data_dir = manifest_dir.join(data_dir);
    if let Err(err) = remove_stale_data_dirs(data_dir.parent().unwrap()) {
        warn!("Failed to remove stale data directories: {err}");
    }

    let instance_dir = data_dir.join(INSTANCE_DIR_NAME);
    fs::create_dir_all(&instance_dir).with_context(|| {
        format!(
            "failed to create instance directory '{}'",
            instance_dir.display()
        )
    })?;

    let picodata_path = picodata_path();
    let host = slot.host();
    let bin_addr = format!("{host}:{}", slot.base_bin_port() + 1);
    let pg_addr = format!("{host}:{}", slot.base_pg_port() + 1);

    let mut command = Command::new(&picodata_path);
    command
        .arg("run")
        .arg("--instance-dir")
        .arg(&instance_dir)
        .args(["--iproto-listen", &bin_addr])
        .args(["--peer", &bin_addr])
        .args(["--pg-listen", &pg_addr])
        // Relative to the working directory, which is the instance directory:
        // the absolute path may exceed the limit on Unix socket path length.
        .args(["--admin-sock", ADMIN_SOCKET_NAME])
        .current_dir(&instance_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    debug!("Starting standalone picodata instance: {command:?}");
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start picodata from '{}'",
            picodata_path.display()
        )
    })?;

    let log_path = instance_dir.join(INSTANCE_LOG_FILE_NAME);
    if let Err(err) = capture_logs(&mut child, &log_path) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    let mut instance = Instance {
        picodata_path,
        socket_path: instance_dir.join(ADMIN_SOCKET_NAME),
        instance_dir,
        child,
        last_test: None,
        _slot: slot,
        _data_dir_lock: data_dir_lock,
    };

    if let Err(err) = wait_admin_socket(&mut instance) {
        let _ = instance.child.kill();
        let _ = instance.child.wait();
        return Err(err);
    }

    Ok(instance)
}

/// Builds the cdylib of the crate under test, the same way pike does for plugins.
fn build_crate(manifest_dir: &Path) -> anyhow::Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let mut command = Command::new(cargo);
    command.args(["build", "--lib"]);
    if use_release_build() {
        command.arg("--release");
    }
    command
        .arg("--target-dir")
        .arg(target_dir(manifest_dir))
        .current_dir(manifest_dir);

    debug!("Building crate under test: {command:?}");
    let status = command
        .status()
        .context("failed to run cargo to build crate under test")?;
    if !status.success() {
        bail!(
            "failed to build crate under test in '{}': cargo exited with {status}",
            manifest_dir.display()
        );
    }

    Ok(())
}

/// Prints instance output to stdout of the test process with an instance
/// prefix and duplicates it to `log_path`.
fn capture_logs(child: &mut Child, log_path: &Path) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path)
        .with_context(|| format!("failed to open log file '{}'", log_path.display()))?;
    let file = Arc::new(Mutex::new(file));

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let outputs: [Box<dyn Read + Send>; 2] = [Box::new(stdout), Box::new(stderr)];

    for output in outputs {
        let file = Arc::clone(&file);
        thread::Builder::new()
            .name("picotest_standalone_log".into())
            .spawn(move || forward_lines(output, &file))
            .context("failed to spawn log forwarding thread")?;
    }

    Ok(())
}

fn forward_lines(output: impl Read, file: &Mutex<File>) {
    let mut reader = BufReader::new(output);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);

        println!("{LOG_PREFIX}{line}");
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{line}");
    }
}

fn wait_admin_socket(instance: &mut Instance) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        // `UnixStream::connect` can't be used here: the absolute socket path
        // may exceed the length limit, and `picodata admin` connects to it
        // relative to the instance directory.
        let probe = run_admin_lua(
            &instance.picodata_path,
            &instance.socket_path,
            b"return true",
        );
        if probe.is_ok_and(|output| output.contains("true")) {
            return Ok(());
        }

        if let Some(status) = instance.child.try_wait()? {
            bail!(
                "picodata instance exited during startup with {status}, see {}",
                instance.log_path().display()
            );
        }

        if start.elapsed() >= ADMIN_SOCKET_TIMEOUT {
            bail!(
                "admin console '{}' is not available after {ADMIN_SOCKET_TIMEOUT:?}, see {}",
                instance.socket_path.display(),
                instance.log_path().display()
            );
        }

        thread::sleep(Duration::from_millis(100));
    }
}
