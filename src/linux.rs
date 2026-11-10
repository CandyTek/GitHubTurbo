//! Linux system integration: the counterpart of `windows.ps1`.
//!
//! `enable` makes these changes, each recorded in `state/session.json` before it is
//! made, so `disable`, `recover`, the watchdog and the boot-time recovery unit can
//! undo exactly what was done:
//!
//! 1. The local CA goes into the distribution's trust store (git, curl, wget and
//!    most programs read it).
//! 2. A ProxyCommand for github.com goes at the top of the invoking user's
//!    `~/.ssh/config`.
//! 3. The engine starts in its own session, so closing the terminal does not stop it.
//! 4. With systemd, a oneshot unit restores everything at the next boot if the
//!    session was never stopped.
//! 5. If Firefox is installed, an enterprise policy in `/etc/firefox/policies` makes
//!    it trust the local CA: Firefox ignores the system trust store on Linux.
//! 6. `/etc/hosts` maps the GitHub names to 127.0.0.1. Existing GitHub entries are
//!    commented out and restored afterwards.
//!
//! Wildcard DNS routing (Windows' NRPT) has no safe equivalent across Linux
//! resolvers, so only the listed names are accelerated; other GitHub names keep
//! working directly.
use super::*;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const HOSTS: &str = "/etc/hosts";
const PREFIX: &str = "# GitHubTurbo saved: ";
const BEGIN: &str = "# GitHubTurbo managed BEGIN";
const END: &str = "# GitHubTurbo managed END";
const RETAINED: &str = " # GitHubTurbo retained";
const UNIT_NAME: &str = "github-turbo-recover.service";
const UNIT_PATH: &str = "/etc/systemd/system/github-turbo-recover.service";
/// Read by Firefox on Linux, including the Snap package, which can read only
/// `/etc/firefox` on the host; so the certificate goes beside the policy.
const FIREFOX_POLICIES: &str = "/etc/firefox/policies";
const FIREFOX_CERT: &str = "/etc/firefox/policies/github-turbo.crt";
/// Key material destroyed by a completed restore. Firefox keeps trusting a CA it
/// imported through the policy even after the policy is gone; without the key that
/// trust can never be used, and the next `enable` creates a new CA.
const CA_FILES: &[&str] = &[
    "ca-key.pem",
    "ca.pem",
    "ca.crl",
    "server.pem",
    "server-key.pem",
];
/// Names mapped to the engine. Others under the GitHub domains resolve as usual.
const KNOWN: &[&str] = &[
    "github.com",
    "www.github.com",
    "api.github.com",
    "uploads.github.com",
    "gist.github.com",
    "ssh.github.com",
    "codeload.github.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
    "objects-origin.githubusercontent.com",
    "github-cloud.githubusercontent.com",
    "avatars.githubusercontent.com",
    "media.githubusercontent.com",
    "gist.githubusercontent.com",
    "user-images.githubusercontent.com",
    "private-user-images.githubusercontent.com",
    "desktop.githubusercontent.com",
    "github.githubassets.com",
];

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Session {
    phase: String,
    engine_pid: u32,
    /// Boot ID and start time of the engine, so a reused PID is never mistaken for it.
    engine_ticks: String,
    /// `state/hosts.original` and `state/hosts.applied` hold the file before and after.
    hosts_applied: bool,
    ca: Option<CaTrust>,
    ssh: Option<SshEntry>,
    recovery_unit: bool,
    firefox: Option<FirefoxPolicy>,
}
/// `state/firefox-policies.original` and `.applied` hold policies.json before and after.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirefoxPolicy {
    existed: bool,
    /// Directories created for the policy, removed again when empty.
    created: Vec<PathBuf>,
}
#[derive(Serialize, Deserialize)]
struct CaTrust {
    path: PathBuf,
    refresh: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SshEntry {
    path: PathBuf,
    existed: bool,
    block: String,
}

struct Context {
    exe: PathBuf,
    base: PathBuf,
    config: Option<serde_json::Value>,
    state: PathBuf,
    listen: SocketAddr,
}
impl Context {
    fn load() -> Result<Self> {
        let exe = std::env::current_exe()?;
        let base = exe
            .parent()
            .ok_or_else(|| err("missing executable directory"))?
            .to_path_buf();
        let config: Option<serde_json::Value> = fs::read(base.join("config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let value = config.clone().unwrap_or_default();
        let state = Path::new(
            value["transparent"]["state_dir"]
                .as_str()
                .unwrap_or("state"),
        );
        let state = if state.is_absolute() {
            state.to_path_buf()
        } else {
            base.join(state)
        };
        let listen = value["listen"]
            .as_str()
            .unwrap_or("127.0.0.1:18080")
            .parse()?;
        Ok(Self {
            exe,
            base,
            config,
            state,
            listen,
        })
    }
    fn session_path(&self) -> PathBuf {
        self.state.join("session.json")
    }
    fn read_session(&self) -> Result<Option<Session>> {
        match fs::read(self.session_path()) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn write_session(&self, session: &Session) -> Result<()> {
        let temp = self.state.join("session.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(session)?)?;
        fs::rename(temp, self.session_path())?;
        Ok(())
    }
    /// True when the recorded engine is still this executable's running process.
    fn engine_alive(&self, session: &Session) -> bool {
        session.engine_pid > 0
            && identity(session.engine_pid)
                .is_some_and(|(exe, ticks)| exe == self.exe && ticks == session.engine_ticks)
    }
}

/// Executable path and boot-qualified start time of a process.
fn identity(pid: u32) -> Option<(PathBuf, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (starttime); the command name in field 2 may contain spaces.
    let start = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let exe = exe.to_string_lossy();
    let exe = PathBuf::from(exe.strip_suffix(" (deleted)").unwrap_or(&exe));
    Some((exe, format!("{}:{start}", boot.trim())))
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Holds the integration lock for the lifetime of the value.
struct Lock(#[allow(dead_code)] fs::File);
fn lock(state: &Path) -> Result<Lock> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("integration.lock"))?;
    for _ in 0..100 {
        match file.try_lock() {
            Ok(()) => return Ok(Lock(file)),
            Err(fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(100)),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Err(err("Another GitHubTurbo operation is in progress"))
}

fn prepare_state(state: &Path) -> Result<()> {
    fs::create_dir_all(state)?;
    // The CA private key lives here: root only.
    fs::set_permissions(state, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// `enable`, `disable` and `recover`.
pub fn run(action: &str) -> Result<()> {
    if !is_root() {
        // Like the Windows elevation prompt: re-run the same command through sudo.
        let exe = std::env::current_exe()?;
        let status = Command::new("sudo")
            .arg("--")
            .arg(&exe)
            .arg(action)
            .status()
            .map_err(|_| err("Root privileges are required; run this command as root"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let context = Context::load()?;
    prepare_state(&context.state)?;
    let _lock = lock(&context.state)?;
    if action == "enable" {
        return enable(&context);
    }
    restore(&context, None)?;
    println!("GitHubTurbo disabled; original settings restored.");
    Ok(())
}

/// `watch PID`: restore the settings when that engine ends without `disable`.
pub fn watch(pid: u32) -> Result<()> {
    let context = Context::load()?;
    match context.read_session()? {
        Some(session) if session.engine_pid == pid => {
            if !context.engine_alive(&session) {
                return Err(err("watchdog process identity mismatch"));
            }
        }
        _ => return Ok(()),
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let Some(session) = context.read_session().ok().flatten() else {
            return Ok(());
        };
        if session.engine_pid != pid {
            return Ok(());
        }
        if !context.engine_alive(&session) {
            let _lock = lock(&context.state)?;
            return restore(&context, Some(pid));
        }
    }
}

fn enable(context: &Context) -> Result<()> {
    let url = format!("http://{}/", context.listen);
    if let Some(session) = context.read_session()? {
        if context.engine_alive(&session) {
            println!("GitHubTurbo is already enabled: {url}");
            return Ok(());
        }
        restore(context, None)?;
    }
    let config = context.config.as_ref().ok_or_else(|| {
        err("Configuration is invalid. `recover` remains available to restore an active session.")
    })?;
    let transparent = &config["transparent"];
    if transparent["enabled"] != true {
        return Err(err("transparent.enabled must be true"));
    }
    if transparent["https_listen"] != "127.0.0.1:443"
        || transparent["http_listen"] != "127.0.0.1:80"
        || transparent["dns_listen"] != "127.0.0.1:53"
    {
        return Err(err(
            "System integration requires the standard loopback HTTPS, HTTP and DNS ports",
        ));
    }
    for address in [context.listen.to_string(), "127.0.0.1:443".into()] {
        if std::net::TcpListener::bind(&address).is_err() {
            return Err(err(&format!(
                "{address} is already in use; stop the conflicting application first"
            )));
        }
    }
    // Plain HTTP and DNS are conveniences: the engine starts without them when the
    // ports belong to a web server or a local resolver.
    let mut notes: Vec<String> = Vec::new();
    if std::net::TcpListener::bind("127.0.0.1:80").is_err() {
        notes.push("Port 80 is used by another program, so http:// GitHub links are not redirected to https://.".into());
    }
    if std::net::UdpSocket::bind("127.0.0.1:53").is_err()
        || std::net::TcpListener::bind("127.0.0.1:53").is_err()
    {
        notes
            .push("Port 53 is used by another program; the diagnostic DNS listener is off.".into());
    }
    let output = Command::new(&context.exe)
        .arg("init-ca")
        .arg(context.base.join("config.json"))
        .stdout(Stdio::null())
        .status()?;
    if !output.success() {
        return Err(err("Certificate initialization failed"));
    }
    for name in ["ca-key.pem", "server-key.pem"] {
        fs::set_permissions(context.state.join(name), fs::Permissions::from_mode(0o600))?;
    }
    let mut session = Session {
        phase: "preparing".into(),
        ..Default::default()
    };
    context.write_session(&session)?;
    match apply(context, &mut session, &mut notes) {
        Ok(()) => {
            println!("GitHubTurbo enabled: {url}");
            for note in notes {
                println!("Note: {note}");
            }
            if let Some(ca) = &session.ca {
                println!(
                    "Programs with their own CA bundle need the local CA too, for example NODE_EXTRA_CA_CERTS={}",
                    ca.path.display()
                );
            }
            Ok(())
        }
        Err(failure) => {
            if let Err(error) = restore(context, None) {
                eprintln!("Restoring after the failure also failed: {error}");
            }
            Err(failure)
        }
    }
}

fn apply(context: &Context, session: &mut Session, notes: &mut Vec<String>) -> Result<()> {
    let ca = fs::read(context.state.join("ca.pem"))?;
    let (dir, name, refresh) = ca_store().ok_or_else(|| {
        err("No supported CA trust store (update-ca-certificates or update-ca-trust) was found")
    })?;
    let path = Path::new(dir).join(name);
    session.ca = Some(CaTrust {
        path: path.clone(),
        refresh: refresh.iter().map(|s| s.to_string()).collect(),
    });
    context.write_session(session)?;
    fs::write(&path, &ca)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    run_quiet(refresh)?;

    if firefox_installed() {
        match fs::read(Path::new(FIREFOX_POLICIES).join("policies.json")) {
            Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() => notes
                .push(
                "Firefox was not set up: /etc/firefox/policies/policies.json is not valid JSON."
                    .into(),
            ),
            original => {
                apply_firefox(context, session, original.ok(), &ca)?;
                notes.push("Restart Firefox so it trusts the local CA.".into());
            }
        }
    }

    let user = target_user()?;
    let ssh_dir = user.home.join(".ssh");
    if !ssh_dir.exists() {
        fs::create_dir_all(&ssh_dir)?;
        fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700))?;
        std::os::unix::fs::chown(&ssh_dir, Some(user.uid), Some(user.gid))?;
    }
    let ssh_path = ssh_dir.join("config");
    let existed = ssh_path.exists();
    let original = if existed {
        fs::read(&ssh_path)?
    } else {
        Vec::new()
    };
    let block = format!(
        "# GitHubTurbo BEGIN\nHost github.com ssh.github.com\n    ProxyCommand \"{}\" ssh-tunnel\nHost *\n# GitHubTurbo END\n",
        context.exe.display()
    );
    fs::write(context.state.join("ssh-config.original"), &original)?;
    session.ssh = Some(SshEntry {
        path: ssh_path.clone(),
        existed,
        block: block.clone(),
    });
    context.write_session(session)?;
    let mut patched = block.into_bytes();
    patched.extend_from_slice(&original);
    fs::write(&ssh_path, patched)?;
    if !existed {
        fs::set_permissions(&ssh_path, fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::chown(&ssh_path, Some(user.uid), Some(user.gid))?;
    }

    let log = |name: &str| fs::File::create(context.state.join(name));
    let mut engine = Command::new(&context.exe);
    engine
        .arg("serve")
        .current_dir(&context.base)
        .stdin(Stdio::null())
        .stdout(log("engine-out.log")?)
        .stderr(log("engine.log")?);
    detach(&mut engine);
    let mut child = engine.spawn()?;
    let pid = child.id();
    session.engine_pid = pid;
    session.engine_ticks = identity(pid).map(|(_, t)| t).unwrap_or_default();
    context.write_session(session)?;
    let mut ready = false;
    for _ in 0..100 {
        if child.try_wait()?.is_some() {
            return Err(err(&format!(
                "Engine exited; see {}",
                context.state.join("engine.log").display()
            )));
        }
        // Ready means this exact process answers, whatever its version.
        if let Some(status) = status(context.listen) {
            if status["name"] == "GitHubTurbo" && status["pid"] == pid {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !ready {
        return Err(err("Engine did not become ready"));
    }

    if Path::new("/run/systemd/system").is_dir() {
        session.recovery_unit = true;
        context.write_session(session)?;
        fs::write(UNIT_PATH, recovery_unit(&context.exe))?;
        run_quiet(&["systemctl", "daemon-reload"])?;
        run_quiet(&["systemctl", "enable", "--quiet", UNIT_NAME])?;
    }

    let original = fs::read(HOSTS)?;
    let patched = patch_hosts(&String::from_utf8_lossy(&original));
    fs::write(context.state.join("hosts.original"), &original)?;
    fs::write(context.state.join("hosts.applied"), &patched)?;
    session.hosts_applied = true;
    context.write_session(session)?;
    // Rewrite in place: /etc/hosts may be a bind mount (containers), and this keeps
    // its owner and mode.
    fs::write(HOSTS, &patched)?;
    flush_dns();

    session.phase = "enabled".into();
    context.write_session(session)?;
    let mut watcher = Command::new(&context.exe);
    watcher
        .arg("watch")
        .arg(pid.to_string())
        .current_dir(&context.base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log("watchdog.log")?);
    detach(&mut watcher);
    let mut watcher = watcher.spawn()?;
    std::thread::sleep(Duration::from_millis(300));
    if watcher.try_wait()?.is_some() {
        return Err(err("Recovery watchdog failed to start"));
    }
    Ok(())
}

/// Undo everything the session records, newest routing change first.
fn restore(context: &Context, owner: Option<u32>) -> Result<()> {
    let Some(session) = context.read_session()? else {
        return Ok(());
    };
    if owner.is_some_and(|pid| pid != session.engine_pid) {
        return Ok(());
    }
    let mut failures = Vec::new();
    let mut attempt = |what: &str, result: Result<()>| {
        if let Err(e) = result {
            failures.push(format!("{what}: {e}"));
        }
    };
    if session.hosts_applied {
        attempt("hosts", restore_hosts(&context.state));
    }
    if session.recovery_unit {
        let _ = run_quiet(&["systemctl", "disable", "--quiet", UNIT_NAME]);
        attempt("recovery unit", remove_file(Path::new(UNIT_PATH)));
        let _ = run_quiet(&["systemctl", "daemon-reload"]);
    }
    if let Some(ssh) = &session.ssh {
        attempt("ssh", restore_ssh(&context.state, ssh));
    }
    if let Some(firefox) = &session.firefox {
        attempt("Firefox policy", restore_firefox(&context.state, firefox));
    }
    if let Some(ca) = &session.ca {
        attempt(
            "CA",
            remove_file(&ca.path).and_then(|()| {
                let refresh: Vec<&str> = ca.refresh.iter().map(String::as_str).collect();
                run_quiet(&refresh)?;
                remove_dangling_links(Path::new("/etc/ssl/certs"), &ca.path)
            }),
        );
    }
    if context.engine_alive(&session) {
        stop(session.engine_pid, context, &session);
    }
    flush_dns();
    if !failures.is_empty() {
        // Keep the session so `recover` can try again.
        return Err(err(&format!(
            "Some settings could not be restored: {}",
            failures.join("; ")
        )));
    }
    for name in [
        "hosts.original",
        "hosts.applied",
        "ssh-config.original",
        "firefox-policies.original",
        "firefox-policies.applied",
    ] {
        let _ = remove_file(&context.state.join(name));
    }
    // Only once nothing refers to the CA any more: the engine has stopped.
    for name in CA_FILES {
        remove_file(&context.state.join(name))?;
    }
    remove_file(&context.session_path())
}

fn firefox_installed() -> bool {
    which("firefox") || which("firefox-esr") || Path::new("/snap/firefox").exists()
}

fn apply_firefox(
    context: &Context,
    session: &mut Session,
    original: Option<Vec<u8>>,
    ca: &[u8],
) -> Result<()> {
    let policies = Path::new(FIREFOX_POLICIES);
    let created: Vec<PathBuf> = [Path::new("/etc/firefox"), policies]
        .iter()
        .filter(|dir| !dir.exists())
        .map(|dir| dir.to_path_buf())
        .collect();
    let existed = original.is_some();
    let original = original.unwrap_or_default();
    let document = if existed {
        serde_json::from_slice(&original)?
    } else {
        serde_json::json!({})
    };
    let patched = serde_json::to_vec_pretty(&add_firefox_policy(document, FIREFOX_CERT)?)?;
    fs::write(context.state.join("firefox-policies.original"), &original)?;
    fs::write(context.state.join("firefox-policies.applied"), &patched)?;
    session.firefox = Some(FirefoxPolicy { existed, created });
    context.write_session(session)?;
    fs::create_dir_all(policies)?;
    fs::write(FIREFOX_CERT, ca)?;
    fs::set_permissions(FIREFOX_CERT, fs::Permissions::from_mode(0o644))?;
    let path = policies.join("policies.json");
    fs::write(&path, patched)?;
    if !existed {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

fn restore_firefox(state: &Path, firefox: &FirefoxPolicy) -> Result<()> {
    let path = Path::new(FIREFOX_POLICIES).join("policies.json");
    if let Ok(current) = fs::read(&path) {
        let applied = fs::read(state.join("firefox-policies.applied")).ok();
        if applied.as_deref() == Some(current.as_slice()) {
            if firefox.existed {
                fs::write(&path, fs::read(state.join("firefox-policies.original"))?)?;
            } else {
                remove_file(&path)?;
            }
        } else if let Ok(document) = serde_json::from_slice(&current) {
            // Edited since: take out only our entry.
            fs::write(
                &path,
                serde_json::to_vec_pretty(&remove_firefox_policy(document, FIREFOX_CERT))?,
            )?;
        }
    }
    remove_file(Path::new(FIREFOX_CERT))?;
    for dir in firefox.created.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// Adds the certificate to `policies.Certificates.Install`, keeping everything else.
pub fn add_firefox_policy(
    mut document: serde_json::Value,
    cert: &str,
) -> Result<serde_json::Value> {
    let install = document
        .as_object_mut()
        .ok_or_else(|| err("policies.json is not a JSON object"))?
        .entry("policies")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| err("policies is not an object"))?
        .entry("Certificates")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| err("policies.Certificates is not an object"))?
        .entry("Install")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| err("policies.Certificates.Install is not a list"))?;
    if !install.iter().any(|entry| entry == cert) {
        install.push(cert.into());
    }
    Ok(document)
}

/// Removes the certificate, and any container left empty by that.
pub fn remove_firefox_policy(mut document: serde_json::Value, cert: &str) -> serde_json::Value {
    let Some(policies) = document.get_mut("policies").and_then(|p| p.as_object_mut()) else {
        return document;
    };
    if let Some(certificates) = policies
        .get_mut("Certificates")
        .and_then(|c| c.as_object_mut())
    {
        if let Some(install) = certificates
            .get_mut("Install")
            .and_then(|i| i.as_array_mut())
        {
            install.retain(|entry| entry != cert);
            if install.is_empty() {
                certificates.remove("Install");
            }
        }
        if certificates.is_empty() {
            policies.remove("Certificates");
        }
    }
    if policies.is_empty() {
        if let Some(root) = document.as_object_mut() {
            root.remove("policies");
        }
    }
    document
}

fn restore_hosts(state: &Path) -> Result<()> {
    let current = fs::read(HOSTS)?;
    let applied = fs::read(state.join("hosts.applied")).ok();
    if applied.as_deref() == Some(current.as_slice()) {
        fs::write(HOSTS, fs::read(state.join("hosts.original"))?)?;
    } else {
        // Edited since: remove only our lines and bring back the saved ones.
        fs::write(HOSTS, strip_hosts(&String::from_utf8_lossy(&current)))?;
    }
    Ok(())
}

fn restore_ssh(state: &Path, ssh: &SshEntry) -> Result<()> {
    let Ok(text) = fs::read(&ssh.path) else {
        return Ok(());
    };
    let Some(remaining) = text.strip_prefix(ssh.block.as_bytes()) else {
        return Ok(());
    };
    if !ssh.existed && remaining.is_empty() {
        return remove_file(&ssh.path);
    }
    let original = fs::read(state.join("ssh-config.original")).unwrap_or_default();
    fs::write(
        &ssh.path,
        if remaining == original.as_slice() {
            &original
        } else {
            remaining
        },
    )?;
    Ok(())
}

fn stop(pid: u32, context: &Context, session: &Session) {
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    for _ in 0..50 {
        if !context.engine_alive(session) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if context.engine_alive(session) {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}

/// Debian's update-ca-certificates leaves the links to a removed local certificate
/// behind (`--fresh` would also drop links other software placed). Remove only the
/// links that lead to `target`: the named link and the hash links pointing at it.
fn remove_dangling_links(dir: &Path, target: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    let links: Vec<(PathBuf, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.path(), fs::read_link(entry.path()).ok()?)))
        .collect();
    let named: Vec<&PathBuf> = links
        .iter()
        .filter(|(_, to)| to == target)
        .map(|(link, _)| link)
        .collect();
    for (link, to) in &links {
        let to = dir.join(to);
        if (to == target || named.contains(&&to)) && !link.exists() {
            remove_file(link)?;
        }
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// New session and no controlling terminal: survives the shell, sudo and SSH logout.
fn detach(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn run_quiet(args: &[&str]) -> Result<()> {
    let (program, rest) = args.split_first().ok_or_else(|| err("empty command"))?;
    let output = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(&format!("{program}: {e}")))?;
    if !output.status.success() {
        return Err(err(&format!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn flush_dns() {
    if which("resolvectl") {
        let _ = run_quiet(&["resolvectl", "flush-caches"]);
    }
    if which("nscd") {
        let _ = run_quiet(&["nscd", "-i", "hosts"]);
    }
}

fn which(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = path
        .split(':')
        .chain(["/usr/sbin", "/sbin", "/usr/bin", "/bin"])
        .any(|dir| Path::new(dir).join(name).is_file());
    found
}

/// Anchor directory, file name and refresh command of the system trust store.
fn ca_store() -> Option<(&'static str, &'static str, &'static [&'static str])> {
    const STORES: &[(&str, &str, &[&str])] = &[
        // Debian, Ubuntu and derivatives
        (
            "/usr/local/share/ca-certificates",
            "github-turbo.crt",
            &["update-ca-certificates"],
        ),
        // Fedora, RHEL, CentOS
        (
            "/etc/pki/ca-trust/source/anchors",
            "github-turbo.pem",
            &["update-ca-trust", "extract"],
        ),
        // Arch
        (
            "/etc/ca-certificates/trust-source/anchors",
            "github-turbo.crt",
            &["update-ca-trust"],
        ),
        // openSUSE
        (
            "/etc/pki/trust/anchors",
            "github-turbo.pem",
            &["update-ca-certificates"],
        ),
    ];
    STORES
        .iter()
        .find(|(dir, _, refresh)| Path::new(dir).is_dir() && which(refresh[0]))
        .copied()
}

struct User {
    home: PathBuf,
    uid: u32,
    gid: u32,
}
/// The user who ran sudo, whose SSH configuration should change; otherwise root.
fn target_user() -> Result<User> {
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|name| !name.is_empty() && name != "root");
    unsafe {
        let entry = match &name {
            Some(name) => {
                let name = std::ffi::CString::new(name.as_str())?;
                libc::getpwnam(name.as_ptr())
            }
            None => libc::getpwuid(0),
        };
        if entry.is_null() {
            return Err(err("Cannot find the home directory of the invoking user"));
        }
        let home = std::ffi::CStr::from_ptr((*entry).pw_dir);
        Ok(User {
            home: PathBuf::from(std::ffi::OsStr::from_encoded_bytes_unchecked(
                home.to_bytes(),
            )),
            uid: (*entry).pw_uid,
            gid: (*entry).pw_gid,
        })
    }
}

fn recovery_unit(exe: &Path) -> String {
    format!(
        "# Removed by GitHubTurbo when it stops normally.\n[Unit]\nDescription=Restore settings changed by an interrupted GitHubTurbo session\nAfter=local-fs.target\n\n[Service]\nType=oneshot\nExecStart=\"{}\" recover\n\n[Install]\nWantedBy=multi-user.target\n",
        exe.display()
    )
}

/// `GET /status` from the control port, blocking.
fn status(listen: SocketAddr) -> Option<serde_json::Value> {
    let mut stream = std::net::TcpStream::connect_timeout(&listen, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET /status HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    serde_json::from_str(text.split_once("\r\n\r\n")?.1).ok()
}

fn managed(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    DOMAINS
        .iter()
        .any(|domain| name == *domain || name.ends_with(&format!(".{domain}")))
}

/// Comment out existing GitHub entries and append the managed block.
pub fn patch_hosts(original: &str) -> String {
    let mut known: Vec<String> = KNOWN.iter().map(|s| s.to_string()).collect();
    let mut rows = Vec::new();
    for line in original.lines() {
        let content = line.split('#').next().unwrap_or("");
        let tokens: Vec<&str> = content.split_whitespace().collect();
        let (mine, other): (Vec<&str>, Vec<&str>) =
            tokens.iter().skip(1).partition(|name| managed(name));
        if mine.is_empty() {
            rows.push(line.to_string());
            continue;
        }
        rows.push(format!("{PREFIX}{line}"));
        if !other.is_empty() {
            rows.push(format!("{} {}{RETAINED}", tokens[0], other.join(" ")));
        }
        for name in mine {
            if !known.iter().any(|k| k.eq_ignore_ascii_case(name)) {
                known.push(name.to_ascii_lowercase());
            }
        }
    }
    rows.push(BEGIN.into());
    rows.extend(known.iter().map(|name| format!("127.0.0.1 {name}")));
    rows.push(END.into());
    rows.join("\n") + "\n"
}

/// Remove the managed lines from a hosts file and bring back the saved ones.
pub fn strip_hosts(text: &str) -> String {
    let mut inside = false;
    let mut rows = Vec::new();
    for line in text.lines() {
        if line == BEGIN {
            inside = true;
        } else if line == END {
            inside = false;
        } else if inside || line.ends_with(RETAINED) {
        } else if let Some(saved) = line.strip_prefix(PREFIX) {
            rows.push(saved.to_string());
        } else {
            rows.push(line.to_string());
        }
    }
    rows.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGINAL: &str =
        "127.0.0.1 localhost\n140.82.112.3 github.com gh.local # pinned\n::1 ip6-localhost\n";

    #[test]
    fn hosts_patch_saves_github_lines_and_maps_known_names() {
        let patched = patch_hosts(ORIGINAL);
        assert!(patched.contains("# GitHubTurbo saved: 140.82.112.3 github.com gh.local # pinned"));
        assert!(patched.contains("\n140.82.112.3 gh.local # GitHubTurbo retained\n"));
        assert!(patched.contains("\n127.0.0.1 raw.githubusercontent.com\n"));
        assert_eq!(patched.matches("127.0.0.1 github.com\n").count(), 1);
        assert!(!patched
            .lines()
            .any(|l| l.starts_with("140.82.112.3 github.com")));
    }

    #[test]
    fn hosts_strip_restores_the_original() {
        assert_eq!(strip_hosts(&patch_hosts(ORIGINAL)), ORIGINAL);
        // Lines the user added while enabled stay.
        let edited = patch_hosts(ORIGINAL) + "10.0.0.1 nas\n";
        assert_eq!(strip_hosts(&edited), format!("{ORIGINAL}10.0.0.1 nas\n"));
    }

    #[test]
    fn firefox_policy_is_merged_and_removed_cleanly() {
        let cert = "/etc/firefox/policies/github-turbo.crt";
        let fresh = add_firefox_policy(serde_json::json!({}), cert).unwrap();
        assert_eq!(
            fresh,
            serde_json::json!({"policies": {"Certificates": {"Install": [cert]}}})
        );
        assert_eq!(remove_firefox_policy(fresh, cert), serde_json::json!({}));
        let admin = serde_json::json!({"policies": {
            "DisableTelemetry": true,
            "Certificates": {"ImportEnterpriseRoots": true, "Install": ["corp.crt"]}
        }});
        let merged = add_firefox_policy(admin.clone(), cert).unwrap();
        assert_eq!(
            merged["policies"]["Certificates"]["Install"],
            serde_json::json!(["corp.crt", cert])
        );
        // Adding twice keeps one entry.
        assert_eq!(add_firefox_policy(merged.clone(), cert).unwrap(), merged);
        assert_eq!(remove_firefox_policy(merged, cert), admin);
        assert!(add_firefox_policy(serde_json::json!([]), cert).is_err());
    }

    #[test]
    fn unrelated_names_are_not_managed() {
        assert!(managed("objects.githubusercontent.com"));
        assert!(managed("GitHub.com"));
        assert!(!managed("notgithub.com"));
        assert!(!managed("github.com.evil.example"));
    }

    #[test]
    fn only_dangling_links_to_the_certificate_are_removed() {
        let dir = std::env::temp_dir().join(format!("gt-links-{}", std::process::id()));
        let certs = dir.join("certs");
        fs::create_dir_all(&certs).unwrap();
        let ours = dir.join("github-turbo.crt");
        let other = dir.join("other.crt");
        fs::write(&other, "x").unwrap();
        let link =
            |to: &Path, name: &str| std::os::unix::fs::symlink(to, certs.join(name)).unwrap();
        link(&ours, "github-turbo.pem");
        link(Path::new("github-turbo.pem"), "8c775b17.0");
        link(&other, "other.pem");
        link(Path::new("other.pem"), "1234abcd.0");
        link(&dir.join("missing.crt"), "unrelated-dangling.pem");
        remove_dangling_links(&certs, &ours).unwrap();
        let mut left: Vec<_> = fs::read_dir(&certs)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["1234abcd.0", "other.pem", "unrelated-dangling.pem"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identity_of_this_process() {
        let (exe, ticks) = identity(std::process::id()).unwrap();
        assert_eq!(exe, std::env::current_exe().unwrap());
        assert!(ticks.contains(':'));
    }
}
