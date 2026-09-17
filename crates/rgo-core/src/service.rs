//! Per-user service integration for the foreground rgo daemon.
//!
//! Rendering is separated from installation so setup tests never need to mutate the host's
//! launchd, systemd, or Task Scheduler state.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::RgoPaths;

#[cfg(target_os = "macos")]
const MAC_LABEL: &str = "com.rgo.daemon";
#[cfg(target_os = "linux")]
const LINUX_UNIT: &str = "rgo-daemon.service";
#[cfg(target_os = "windows")]
const WINDOWS_TASK: &str = "rgo\\daemon";

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

pub fn render(executable: &Path) -> Result<RenderedService> {
    let executable = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        let home = directories::UserDirs::new()
            .context("cannot determine home directory")?
            .home_dir()
            .to_path_buf();
        let path = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{MAC_LABEL}.plist"));
        let logs = RgoPaths::discover()?.logs_dir();
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{MAC_LABEL}</string>\n\
  <key>ProgramArguments</key>\n  <array><string>{}</string><string>daemon</string><string>--foreground</string></array>\n\
  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n\
  <key>StandardOutPath</key><string>{}</string>\n\
  <key>StandardErrorPath</key><string>{}</string>\n\
</dict>\n</plist>\n",
            xml_escape(&executable.display().to_string()),
            xml_escape(&logs.join("daemon.stdout.log").display().to_string()),
            xml_escape(&logs.join("daemon.stderr.log").display().to_string()),
        );
        Ok(RenderedService {
            path,
            contents,
            label: MAC_LABEL.into(),
        })
    }
    #[cfg(target_os = "linux")]
    {
        let home = directories::UserDirs::new()
            .context("cannot determine home directory")?
            .home_dir()
            .to_path_buf();
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let path = config.join("systemd").join("user").join(LINUX_UNIT);
        let contents = format!(
            "[Unit]\nDescription=rgo build storage daemon\nAfter=default.target\n\n[Service]\nExecStart={} daemon --foreground\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
            systemd_escape(&executable.display().to_string())
        );
        Ok(RenderedService {
            path,
            contents,
            label: LINUX_UNIT.into(),
        })
    }
    #[cfg(target_os = "windows")]
    {
        let contents = format!(
            "schtasks.exe /Create /TN \"{WINDOWS_TASK}\" /SC ONLOGON /TR \"\\\"{}\\\" daemon --foreground\" /RL LIMITED /F",
            executable.display()
        );
        Ok(RenderedService {
            path: PathBuf::from(WINDOWS_TASK),
            contents,
            label: WINDOWS_TASK.into(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("per-user daemon services are unsupported on this platform")
}

pub fn install(executable: &Path, paths: &RgoPaths) -> Result<RenderedService> {
    let rendered = render(executable)?;
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
        command("systemctl", ["--user", "enable", "--now", LINUX_UNIT])?;
        Ok(rendered)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = paths;
        let task = format!("\"{}\" daemon --foreground", executable.display());
        command(
            "schtasks.exe",
            [
                "/Create",
                "/TN",
                WINDOWS_TASK,
                "/SC",
                "ONLOGON",
                "/TR",
                &task,
                "/RL",
                "LIMITED",
                "/F",
            ],
        )?;
        Ok(rendered)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = paths;
        bail!("per-user daemon services are unsupported on this platform")
    }
}

pub fn uninstall(executable: &Path) -> Result<()> {
    let rendered = render(executable)?;
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe_get_uid());
        let _ = command("launchctl", ["bootout", &domain, &rendered.label]);
        let _ = std::fs::remove_file(&rendered.path);
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let _ = command("systemctl", ["--user", "disable", "--now", LINUX_UNIT]);
        let _ = std::fs::remove_file(&rendered.path);
        let _ = command("systemctl", ["--user", "daemon-reload"]);
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let _ = &rendered;
        let _ = command("schtasks.exe", ["/Delete", "/TN", WINDOWS_TASK, "/F"]);
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("per-user daemon services are unsupported on this platform")
}

pub fn status(executable: &Path) -> Result<ServiceStatus> {
    let rendered = match render(executable) {
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
        let running = command("systemctl", ["--user", "is-active", "--quiet", LINUX_UNIT]).is_ok();
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
        let running = command("schtasks.exe", ["/Query", "/TN", WINDOWS_TASK]).is_ok();
        Ok(ServiceStatus {
            supported: true,
            installed: running,
            running,
            location: rendered.path,
            detail: if running {
                "registered"
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
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
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

#[cfg(unix)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_rendering_contains_foreground_daemon() {
        let rendered = render(Path::new("/usr/local/bin/rgo")).unwrap();
        assert!(rendered.contents.contains("daemon"));
        assert!(rendered.contents.contains("foreground"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xml_escape_is_non_interpreting() {
        assert_eq!(xml_escape("a<&\""), "a&lt;&amp;&quot;");
    }
}
