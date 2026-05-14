use std::{
    ffi::OsStr,
    io,
    path::Path,
    process::{Child, ExitStatus, Output, Stdio},
};

use crate::{platform, sandbox::SandboxPolicy};

/// A sandboxed process builder, providing fine-grained control over how a new
/// process should be spawned.
///
/// This mirrors the API of `std::process::Command` but adds the ability to
/// apply a sandbox policy that restricts filesystem access.
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
/// use sandbox_run::{Command, Sandbox, ProtectSystem};
///
/// let sandbox = Sandbox::new()
///     .protect_system(ProtectSystem::Strict)
///     .read_write_paths([Path::new("/tmp")])
///     .build()
///     .unwrap();
///
/// let output = Command::new("ls")
///     .arg("/usr")
///     .sandbox(sandbox)
///     .output()
///     .expect("failed to execute process");
/// ```
pub struct Command {
    inner: std::process::Command,
    sandbox: Option<SandboxPolicy>,
}

impl Command {
    /// Constructs a new `Command` for launching the program at path `program`.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            inner: std::process::Command::new(program),
            sandbox: None,
        }
    }

    /// Adds an argument to pass to the program.
    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.inner.arg(arg);
        self
    }

    /// Adds multiple arguments to pass to the program.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Inserts or updates an environment variable mapping.
    pub fn env<K, V>(mut self, key: K, val: V) -> Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    /// Adds or updates multiple environment variable mappings.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    /// Removes an environment variable mapping.
    pub fn env_remove<K: AsRef<OsStr>>(mut self, key: K) -> Self {
        self.inner.env_remove(key);
        self
    }

    /// Clears the entire environment map for the child process.
    pub fn env_clear(mut self) -> Self {
        self.inner.env_clear();
        self
    }

    /// Sets the working directory for the child process.
    pub fn current_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        self.inner.current_dir(dir);
        self
    }

    /// Configuration for the child process's standard input (stdin) handle.
    pub fn stdin<T: Into<Stdio>>(mut self, cfg: T) -> Self {
        self.inner.stdin(cfg);
        self
    }

    /// Configuration for the child process's standard output (stdout) handle.
    pub fn stdout<T: Into<Stdio>>(mut self, cfg: T) -> Self {
        self.inner.stdout(cfg);
        self
    }

    /// Configuration for the child process's standard error (stderr) handle.
    pub fn stderr<T: Into<Stdio>>(mut self, cfg: T) -> Self {
        self.inner.stderr(cfg);
        self
    }

    /// Apply a sandbox policy to this command.
    ///
    /// The sandbox will be applied to the child process after fork but before exec,
    /// restricting the process according to the policy.
    pub fn sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(policy);
        self
    }

    /// Executes the command as a child process, returning a handle to it.
    pub fn spawn(mut self) -> io::Result<Child> {
        if let Some(policy) = self.sandbox.take() {
            self.apply_sandbox_pre_exec(policy)?;
        }
        self.inner.spawn()
    }

    /// Executes the command as a child process, waiting for it to finish and
    /// collecting all of its output.
    pub fn output(mut self) -> io::Result<Output> {
        if let Some(policy) = self.sandbox.take() {
            self.apply_sandbox_pre_exec(policy)?;
        }
        self.inner.output()
    }

    /// Executes a command as a child process, waiting for it to finish and
    /// collecting its status.
    pub fn status(mut self) -> io::Result<ExitStatus> {
        if let Some(policy) = self.sandbox.take() {
            self.apply_sandbox_pre_exec(policy)?;
        }
        self.inner.status()
    }

    #[cfg(unix)]
    fn apply_sandbox_pre_exec(&mut self, policy: SandboxPolicy) -> io::Result<()> {
        use std::os::unix::process::CommandExt;

        // SAFETY: The pre_exec closure runs in the child process after fork
        // but before exec. We apply the sandbox here so the executed program
        // runs under the sandbox restrictions.
        unsafe {
            self.inner
                .pre_exec(move || platform::apply_sandbox(&policy).map_err(io::Error::other));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn apply_sandbox_pre_exec(&mut self, _policy: SandboxPolicy) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Sandboxing is only supported on Unix platforms (Linux and macOS)",
        ))
    }
}
