//! Per-user service integration for the foreground rgo daemon.
//!
//! Rendering is separated from installation so setup tests never need to mutate the host's
//! launchd, systemd, or Task Scheduler state.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(any(target_os = "windows", test))]
use anyhow::ensure;
use anyhow::{Context, Result, bail};

use crate::paths::RgoPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Scoped,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedService {
    pub path: PathBuf,
    pub contents: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub supported: bool,
    pub installed: bool,
    pub running: bool,
    pub location: PathBuf,
    pub detail: String,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
struct TaskAction {
    command: String,
    arguments: String,
}

pub fn render(executable: &Path) -> Result<RenderedService> {
    render_flavor(executable, Flavor::Scoped)
}

pub fn render_legacy(executable: &Path) -> Result<RenderedService> {
    render_flavor(executable, Flavor::Legacy)
}

fn render_flavor(executable: &Path, flavor: Flavor) -> Result<RenderedService> {
    let executable = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.to_path_buf());
    let paths = RgoPaths::discover()?;
    let suffix = service_key(&paths.root);
    #[cfg(target_os = "macos")]
    {
        let label = match flavor {
            Flavor::Scoped => format!("com.rgo.daemon.{suffix}"),
            Flavor::Legacy => "com.rgo.daemon".into(),
        };
        let home = directories::UserDirs::new()
            .context("cannot determine home directory")?
            .home_dir()
            .to_path_buf();
        let path = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"));
        let logs = paths.logs_dir();
        let (stdout, stderr) = match flavor {
            Flavor::Scoped => (PathBuf::from("/dev/null"), PathBuf::from("/dev/null")),
            Flavor::Legacy => (
                logs.join("daemon.stdout.log"),
                logs.join("daemon.stderr.log"),
            ),
        };
        let environment = format!(
            "  <key>EnvironmentVariables</key><dict><key>RGO_HOME</key><string>{}</string></dict>\n",
            xml_escape(&paths.root.display().to_string())
        );
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array><string>{}</string><string>daemon</string><string>--foreground</string></array>\n\
{}\
  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n\
  <key>StandardOutPath</key><string>{}</string>\n\
  <key>StandardErrorPath</key><string>{}</string>\n\
</dict>\n</plist>\n",
            xml_escape(&executable.display().to_string()),
            environment,
            xml_escape(&stdout.display().to_string()),
            xml_escape(&stderr.display().to_string()),
        );
        Ok(RenderedService {
            path,
            contents,
            label,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let label = match flavor {
            Flavor::Scoped => format!("rgo-daemon-{suffix}.service"),
            Flavor::Legacy => "rgo-daemon.service".into(),
        };
        let home = directories::UserDirs::new()
            .context("cannot determine home directory")?
            .home_dir()
            .to_path_buf();
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let path = config.join("systemd").join("user").join(&label);
        let environment = match flavor {
            Flavor::Scoped => format!(
                "Environment={}\n",
                systemd_quote(&format!("RGO_HOME={}", paths.root.display()), false)
            ),
            Flavor::Legacy => format!(
                "Environment=RGO_HOME={}\n",
                systemd_escape(&paths.root.display().to_string())
            ),
        };
        let command = match flavor {
            Flavor::Scoped => systemd_quote(&executable.display().to_string(), true),
            Flavor::Legacy => systemd_escape(&executable.display().to_string()),
        };
        let contents = format!(
            "[Unit]\nDescription=rgo build storage daemon\nAfter=default.target\n\n[Service]\nExecStart={} daemon --foreground\n{}Restart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
            command, environment
        );
        Ok(RenderedService {
            path,
            contents,
            label,
        })
    }
    #[cfg(target_os = "windows")]
    {
        let label = match flavor {
            Flavor::Scoped => format!("rgo\\daemon-{suffix}"),
            Flavor::Legacy => "rgo\\daemon".into(),
        };
        let task = match flavor {
            Flavor::Scoped => format!(
                "\"{}\" daemon --foreground --home \"{}\"",
                executable.display(),
                paths.root.display()
            ),
            Flavor::Legacy => format!("\"{}\" daemon --foreground", executable.display()),
        };
        let contents =
            format!("schtasks.exe /Create /TN \"{label}\" /SC ONLOGON /TR \"{task}\" /RL LIMITED");
        Ok(RenderedService {
            path: PathBuf::from(&label),
            contents,
            label,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("per-user daemon services are unsupported on this platform")
}

fn service_key(root: &Path) -> String {
    let fingerprint = blake3::hash(root.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    fingerprint[..16].to_owned()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn legacy_definition_without_home(contents: &str, root: &Path) -> String {
    #[cfg(target_os = "macos")]
    let environment = format!(
        "  <key>EnvironmentVariables</key><dict><key>RGO_HOME</key><string>{}</string></dict>\n",
        xml_escape(&root.display().to_string())
    );
    #[cfg(target_os = "linux")]
    let environment = format!(
        "Environment=RGO_HOME={}\n",
        systemd_escape(&root.display().to_string())
    );
    contents.replacen(&environment, "", 1)
}

#[cfg(target_os = "macos")]
fn former_scoped_macos_logs_definition(contents: &str, root: &Path) -> String {
    let logs = RgoPaths {
        root: root.to_path_buf(),
    }
    .logs_dir();
    contents
        .replace(
            "<key>StandardOutPath</key><string>/dev/null</string>",
            &format!(
                "<key>StandardOutPath</key><string>{}</string>",
                xml_escape(&logs.join("daemon.stdout.log").display().to_string())
            ),
        )
        .replace(
            "<key>StandardErrorPath</key><string>/dev/null</string>",
            &format!(
                "<key>StandardErrorPath</key><string>{}</string>",
                xml_escape(&logs.join("daemon.stderr.log").display().to_string())
            ),
        )
}

#[cfg(target_os = "linux")]
fn former_scoped_definition(executable: &Path, root: &Path) -> String {
    let executable = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.to_path_buf());
    format!(
        "[Unit]\nDescription=rgo build storage daemon\nAfter=default.target\n\n[Service]\nExecStart={} daemon --foreground\nEnvironment=RGO_HOME={}\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(&executable.display().to_string()),
        systemd_escape(&root.display().to_string())
    )
}

#[cfg(any(target_os = "windows", test))]
fn parse_task_action(xml: &str) -> Result<TaskAction> {
    let document = roxmltree::Document::parse(xml).context("parsing scheduled task XML")?;
    let task = document.root_element();
    ensure!(
        task.tag_name().name() == "Task",
        "expected a Task definition"
    );
    let actions: Vec<_> = task
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "Actions")
        .collect();
    ensure!(
        actions.len() == 1,
        "expected one scheduled task Actions element"
    );
    let executions: Vec<_> = actions[0]
        .children()
        .filter(|node| node.is_element())
        .collect();
    ensure!(
        executions.len() == 1 && executions[0].tag_name().name() == "Exec",
        "expected one executable scheduled task action"
    );
    let field = |name| {
        executions[0]
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == name)
            .and_then(|node| node.text())
            .map(str::to_owned)
    };
    Ok(TaskAction {
        command: field("Command").context("scheduled task has no Command")?,
        arguments: field("Arguments").unwrap_or_default(),
    })
}

#[cfg(target_os = "windows")]
fn task_action_matches(action: &TaskAction, executable: &Path, flavor: Flavor) -> Result<bool> {
    // `canonicalize` adds Windows' extended-length `\\?\` prefix, while
    // Task Scheduler commonly exports an ordinary drive path. Compare the
    // registered absolute spelling to the path we supplied at creation.
    let normalize = |path: &str| {
        path.trim()
            .trim_matches('"')
            .strip_prefix(r"\\?\")
            .unwrap_or(path.trim().trim_matches('"'))
            .replace('/', "\\")
    };
    let paths = RgoPaths::discover()?;
    let arguments = match flavor {
        Flavor::Scoped => format!("daemon --foreground --home \"{}\"", paths.root.display()),
        Flavor::Legacy => "daemon --foreground".into(),
    };
    Ok(
        normalize(&action.command).eq_ignore_ascii_case(&normalize(&executable.to_string_lossy()))
            && action.arguments.trim() == arguments,
    )
}

#[cfg(target_os = "windows")]
fn query_task_action(label: &str) -> Result<Option<TaskAction>> {
    let output = Command::new("schtasks.exe")
        .args(["/Query", "/TN", label, "/XML", "/HRESULT"])
        .output()
        .context("querying Task Scheduler")?;
    if matches!(
        output.status.code().map(|code| code as u32),
        Some(0x8007_0002 | 0x8007_0003)
    ) {
        return Ok(None);
    }
    // On current Windows runners, schtasks exits with code 1 and a textual
    // not-found error even with /HRESULT. An absent task is safe to treat as
    // absent; every other query failure still blocks service mutation.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(1)
        && matches!(
            stderr.trim(),
            "ERROR: The system cannot find the path specified."
                | "ERROR: The system cannot find the file specified."
        )
    {
        return Ok(None);
    }
    if !output.status.success() {
        bail!("querying Task Scheduler entry {label}: {}", stderr.trim());
    }
    let xml = decode_task_xml(&output.stdout)?;
    parse_task_action(&xml).map(Some)
}

#[cfg(target_os = "windows")]
fn task_is_running(label: &str) -> Result<bool> {
    // Query the scheduler's state rather than assuming registration means a
    // daemon process exists. The numeric TASK_STATE value is locale-neutral;
    // 4 is TASK_STATE_RUNNING (one or more instances are running).
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$task = Get-ScheduledTask -TaskPath '\\' -TaskName $env:RGO_TASK_LABEL -ErrorAction Stop; [int]$task.State",
        ])
        .env("RGO_TASK_LABEL", label)
        .output()
        .with_context(|| format!("querying Task Scheduler state for {label}"))?;
    if !output.status.success() {
        bail!(
            "querying Task Scheduler state for {label}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let state: u32 = String::from_utf8(output.stdout)?
        .trim()
        .parse()
        .with_context(|| format!("decoding Task Scheduler state for {label}"))?;
    ensure!(
        state <= 4,
        "unexpected Task Scheduler state {state} for {label}"
    );
    Ok(state == 4)
}

#[cfg(any(target_os = "windows", test))]
fn decode_task_xml(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16_task_xml(&bytes[2..], false);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16_task_xml(&bytes[2..], true);
    }
    if bytes.len() >= 8 && bytes[1] == 0 && bytes[3] == 0 && bytes[5] == 0 {
        return decode_utf16_task_xml(bytes, false);
    }
    String::from_utf8(bytes.to_vec()).context("decoding UTF-8 task XML")
}

#[cfg(any(target_os = "windows", test))]
fn decode_utf16_task_xml(bytes: &[u8], big_endian: bool) -> Result<String> {
    ensure!(bytes.len() % 2 == 0, "truncated UTF-16 task XML");
    let units = bytes
        .chunks_exact(2)
        .map(|unit| {
            let pair = [unit[0], unit[1]];
            if big_endian {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).context("decoding UTF-16 task XML")
}

/// Refuse to replace or remove an existing per-user service definition unless it is
/// byte-for-byte the one this installation previously wrote. This guard must
/// run before setup exposes any new Cargo configuration.
pub fn verify_service_ownership(
    executable: &Path,
    previous_executable: Option<&Path>,
) -> Result<()> {
    verify_service_ownership_flavor(executable, previous_executable, Flavor::Scoped)
}

pub fn verify_legacy_service_ownership(
    executable: &Path,
    previous_executable: Option<&Path>,
) -> Result<()> {
    verify_service_ownership_flavor(executable, previous_executable, Flavor::Legacy)
}

fn verify_service_ownership_flavor(
    executable: &Path,
    previous_executable: Option<&Path>,
    flavor: Flavor,
) -> Result<()> {
    let rendered = render_flavor(executable, flavor)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let existing = match std::fs::read_to_string(&rendered.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if status_flavor(executable, flavor)?.running {
                    bail!(
                        "{} is absent but its rgo service name is already running; refusing to replace an unowned daemon",
                        rendered.path.display()
                    );
                }
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", rendered.path.display()));
            }
        };
        if let Some(previous_executable) = previous_executable {
            let previous = render_flavor(previous_executable, flavor)?;
            let root = RgoPaths::discover()?.root;
            let matches = |candidate: &str| {
                existing == candidate
                    || (flavor == Flavor::Legacy
                        && existing == legacy_definition_without_home(candidate, &root))
            };
            #[cfg(target_os = "linux")]
            let matches_former_scoped = flavor == Flavor::Scoped
                && (existing == former_scoped_definition(executable, &root)
                    || existing == former_scoped_definition(previous_executable, &root));
            #[cfg(target_os = "macos")]
            let matches_former_scoped = flavor == Flavor::Scoped
                && (existing == former_scoped_macos_logs_definition(&rendered.contents, &root)
                    || existing == former_scoped_macos_logs_definition(&previous.contents, &root));
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let matches_former_scoped = false;
            if matches(&rendered.contents) || matches(&previous.contents) || matches_former_scoped {
                return Ok(());
            }
        }
        bail!(
            "{} already contains a different rgo service definition; refusing to replace another installation's maintenance service",
            rendered.path.display()
        )
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(action) = query_task_action(&rendered.label)? {
            let owned = if let Some(previous) = previous_executable {
                task_action_matches(&action, executable, flavor)?
                    || task_action_matches(&action, previous, flavor)?
            } else {
                false
            };
            ensure!(
                owned,
                "Task Scheduler entry {} has an unowned action (command {:?}, arguments {:?}); refusing to replace or remove it",
                rendered.label,
                action.command,
                action.arguments
            );
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = previous_executable;
        bail!("per-user daemon services are unsupported on this platform")
    }
}

pub fn install(
    executable: &Path,
    paths: &RgoPaths,
    previous_executable: Option<&Path>,
) -> Result<RenderedService> {
    verify_service_ownership(executable, previous_executable)?;
    let rendered = render(executable)?;
    #[cfg(target_os = "linux")]
    let _ = paths;
    #[cfg(target_os = "macos")]
    {
        std::fs::create_dir_all(
            rendered
                .path
                .parent()
                .context("launchd path has no parent")?,
        )?;
        std::fs::create_dir_all(paths.logs_dir())?;
        atomic_write(&rendered.path, rendered.contents.as_bytes())?;
        let domain = format!("gui/{}", unsafe_get_uid());
        let _ = command("launchctl", ["bootout", &domain, &rendered.label]);
        command(
            "launchctl",
            [
                "bootstrap",
                &domain,
                rendered.path.to_str().unwrap_or_default(),
            ],
        )?;
        Ok(rendered)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::create_dir_all(
            rendered
                .path
                .parent()
                .context("systemd unit path has no parent")?,
        )?;
        atomic_write(&rendered.path, rendered.contents.as_bytes())?;
        command("systemctl", ["--user", "daemon-reload"])?;
        command("systemctl", ["--user", "enable", "--now", &rendered.label])?;
        Ok(rendered)
    }
    #[cfg(target_os = "windows")]
    {
        let task = format!(
            "\"{}\" daemon --foreground --home \"{}\"",
            executable.display(),
            paths.root.display()
        );
        let mut args = vec![
            "/Create",
            "/TN",
            &rendered.label,
            "/SC",
            "ONLOGON",
            "/TR",
            &task,
            "/RL",
            "LIMITED",
        ];
        if previous_executable.is_some() {
            args.push("/F");
        }
        command("schtasks.exe", args)?;
        // ONLOGON only registers the next session. Activate the task now so
        // setup can verify an actual daemon before reporting maintenance ready.
        command("schtasks.exe", ["/Run", "/TN", &rendered.label])?;
        Ok(rendered)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = paths;
        bail!("per-user daemon services are unsupported on this platform")
    }
}

/// Detect the exact older scoped plist that directed launchd output into
/// uncapped files. The caller verifies ownership before replacing the plist.
pub fn scoped_service_used_legacy_logs(
    executable: &Path,
    previous_executable: Option<&Path>,
) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let rendered = render(executable)?;
        let existing = match std::fs::read_to_string(&rendered.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", rendered.path.display()));
            }
        };
        let root = RgoPaths::discover()?.root;
        if existing == former_scoped_macos_logs_definition(&rendered.contents, &root) {
            return Ok(true);
        }
        if let Some(previous_executable) = previous_executable {
            let previous = render(previous_executable)?;
            return Ok(existing == former_scoped_macos_logs_definition(&previous.contents, &root));
        }
        Ok(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (executable, previous_executable);
        Ok(false)
    }
}

/// Remove uncapped launchd logs only after setup has confirmed a healthy
/// replacement for an owned service that actually wrote those files.
pub fn prune_legacy_logs(paths: &RgoPaths, prior_service_wrote_logs: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    if prior_service_wrote_logs {
        for name in ["daemon.stdout.log", "daemon.stderr.log"] {
            remove_if_exists(&paths.logs_dir().join(name))?;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (paths, prior_service_wrote_logs);
    Ok(())
}

pub fn uninstall(executable: &Path, previous_executable: Option<&Path>) -> Result<()> {
    verify_service_ownership(executable, previous_executable)?;
    uninstall_flavor(executable, Flavor::Scoped)
}

pub fn uninstall_legacy(executable: &Path, previous_executable: Option<&Path>) -> Result<()> {
    verify_legacy_service_ownership(executable, previous_executable)?;
    uninstall_flavor(executable, Flavor::Legacy)
}

fn uninstall_flavor(executable: &Path, flavor: Flavor) -> Result<()> {
    let rendered = render_flavor(executable, flavor)?;
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe_get_uid());
        if command(
            "launchctl",
            ["print", &format!("{domain}/{}", rendered.label)],
        )
        .is_ok()
        {
            command("launchctl", ["bootout", &domain, &rendered.label])?;
        }
        remove_if_exists(&rendered.path)?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let installed = rendered.path.is_file();
        let running = command(
            "systemctl",
            ["--user", "is-active", "--quiet", &rendered.label],
        )
        .is_ok();
        if installed || running {
            command("systemctl", ["--user", "disable", "--now", &rendered.label])?;
            remove_if_exists(&rendered.path)?;
            command("systemctl", ["--user", "daemon-reload"])?;
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        if query_task_action(&rendered.label)?.is_some() {
            // /End may report an error when the task is registered but idle.
            let _ = command("schtasks.exe", ["/End", "/TN", &rendered.label]);
            command("schtasks.exe", ["/Delete", "/TN", &rendered.label, "/F"])?;
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("per-user daemon services are unsupported on this platform")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

pub fn status(executable: &Path) -> Result<ServiceStatus> {
    status_flavor(executable, Flavor::Scoped)
}

pub fn legacy_status(executable: &Path) -> Result<ServiceStatus> {
    status_flavor(executable, Flavor::Legacy)
}

fn status_flavor(executable: &Path, flavor: Flavor) -> Result<ServiceStatus> {
    let rendered = match render_flavor(executable, flavor) {
        Ok(value) => value,
        Err(error) => {
            return Ok(ServiceStatus {
                supported: false,
                installed: false,
                running: false,
                location: PathBuf::new(),
                detail: error.to_string(),
            });
        }
    };
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe_get_uid());
        let running = command(
            "launchctl",
            ["print", &format!("{domain}/{}", rendered.label)],
        )
        .is_ok();
        Ok(ServiceStatus {
            supported: true,
            installed: rendered.path.is_file(),
            running,
            location: rendered.path,
            detail: if running { "running" } else { "not running" }.into(),
        })
    }
    #[cfg(target_os = "linux")]
    {
        if Command::new("systemctl").arg("--version").output().is_err() {
            return Ok(ServiceStatus {
                supported: false,
                installed: rendered.path.is_file(),
                running: false,
                location: rendered.path,
                detail: "systemd user manager unavailable; install systemd or run `rgo setup --no-service`".into(),
            });
        }
        let running = command(
            "systemctl",
            ["--user", "is-active", "--quiet", &rendered.label],
        )
        .is_ok();
        Ok(ServiceStatus {
            supported: true,
            installed: rendered.path.is_file(),
            running,
            location: rendered.path,
            detail: if running { "running" } else { "not running" }.into(),
        })
    }
    #[cfg(target_os = "windows")]
    {
        let installed = query_task_action(&rendered.label)?.is_some();
        let running = installed && task_is_running(&rendered.label)?;
        Ok(ServiceStatus {
            supported: true,
            installed,
            running,
            location: rendered.path,
            detail: if running {
                "running"
            } else if installed {
                "registered but not running"
            } else {
                "not registered"
            }
            .into(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    Ok(ServiceStatus {
        supported: false,
        installed: false,
        running: false,
        location: rendered.path,
        detail: "unsupported".into(),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("service path has no parent")?;
    let temp = parent.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&temp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temp, path)?;
    Ok(())
}

fn command<I, S>(program: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new(program).args(args).output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => Err(error).with_context(|| format!("running {program}")),
    }
}

#[cfg(target_os = "macos")]
fn unsafe_get_uid() -> u32 {
    std::process::Command::new("id")
        .args(["-u"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn systemd_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\ ")
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str, executable: bool) -> String {
    let mut value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if executable {
        value = value.replace('$', "$$");
    }
    format!("\"{value}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_rendering_contains_foreground_daemon() {
        let rendered = render(Path::new("/usr/local/bin/rgo")).unwrap();
        assert!(rendered.contents.contains("daemon"));
        assert!(rendered.contents.contains("foreground"));
        #[cfg(target_os = "macos")]
        {
            assert!(
                rendered
                    .contents
                    .contains("<key>StandardOutPath</key><string>/dev/null</string>")
            );
            let root = RgoPaths::discover().unwrap().root;
            let former = former_scoped_macos_logs_definition(&rendered.contents, &root);
            assert!(former.contains("daemon.stdout.log"));
            assert!(former.contains("daemon.stderr.log"));
        }
    }

    #[test]
    fn service_identity_is_stable_per_storage_root() {
        let first = service_key(Path::new("/private/rgo-one"));
        assert_eq!(first, service_key(Path::new("/private/rgo-one")));
        assert_ne!(first, service_key(Path::new("/private/rgo-two")));
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn task_action_parser_decodes_windows_paths_and_rejects_extra_actions() {
        let xml = r#"<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Actions Context="Author"><Exec><Command>C:\Program Files\rgo\rgo.exe</Command><Arguments>daemon --foreground --home &quot;C:\Users\Me\.rgo&quot;</Arguments></Exec></Actions></Task>"#;
        assert_eq!(
            parse_task_action(xml).unwrap(),
            TaskAction {
                command: r"C:\Program Files\rgo\rgo.exe".into(),
                arguments: r#"daemon --foreground --home "C:\Users\Me\.rgo""#.into(),
            }
        );
        let changed = xml.replace("</Actions>", "<ComHandler/><Exec/></Actions>");
        assert!(parse_task_action(&changed).is_err());
    }

    #[test]
    fn task_xml_decoding_accepts_utf8_and_utf16_exports() {
        let xml = "<Task><Actions><Exec><Command>rgo.exe</Command></Exec></Actions></Task>";
        assert_eq!(decode_task_xml(xml.as_bytes()).unwrap(), xml);
        let mut utf16_le = vec![0xff, 0xfe];
        for unit in xml.encode_utf16() {
            utf16_le.extend(unit.to_le_bytes());
        }
        assert_eq!(decode_task_xml(&utf16_le).unwrap(), xml);
        assert_eq!(decode_task_xml(&utf16_le[2..]).unwrap(), xml);
        assert!(decode_task_xml(&utf16_le[..utf16_le.len() - 1]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_quoted_paths_preserve_spaces_and_specifier_characters() {
        assert_eq!(
            systemd_quote("RGO_HOME=/home/me/rgo % cache", false),
            "\"RGO_HOME=/home/me/rgo %% cache\""
        );
        assert_eq!(
            systemd_quote("/home/me/rgo $%/bin/rgo", true),
            "\"/home/me/rgo $$%%/bin/rgo\""
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn former_scoped_systemd_unit_remains_recognizable_during_upgrade() {
        let old = former_scoped_definition(
            Path::new("/home/me/rgo tools/rgo"),
            Path::new("/home/me/rgo data"),
        );
        assert!(old.contains("ExecStart=/home/me/rgo\\ tools/rgo daemon --foreground"));
        assert!(old.contains("Environment=RGO_HOME=/home/me/rgo\\ data"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xml_escape_is_non_interpreting() {
        assert_eq!(xml_escape("a<&\""), "a&lt;&amp;&quot;");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn legacy_definition_can_match_an_install_without_explicit_home() {
        let root = Path::new("/home/me/.rgo");
        #[cfg(target_os = "macos")]
        let with_home = "begin\n  <key>EnvironmentVariables</key><dict><key>RGO_HOME</key><string>/home/me/.rgo</string></dict>\nend\n";
        #[cfg(target_os = "linux")]
        let with_home = "begin\nEnvironment=RGO_HOME=/home/me/.rgo\nend\n";
        assert_eq!(
            legacy_definition_without_home(with_home, root),
            "begin\nend\n"
        );
    }
}
