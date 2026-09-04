//! macOS-only: install/uninstall the "Squeezed" virtual audio device driver.
//!
//! The HAL plug-in compiled by build.rs is embedded in this binary, so a
//! single `sudo squeezed driver install` writes the bundle to the system HAL
//! directory and restarts coreaudiod — no separate download. The device then
//! appears as "Squeezed" in System Settings → Sound → Output.

#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Where coreaudiod looks for HAL plug-ins.
const BUNDLE_PATH: &str = "/Library/Audio/Plug-Ins/HAL/SqueezedAudio.driver";

static DRIVER_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/SqueezedAudio"));
static INFO_PLIST: &[u8] = include_bytes!("../driver/Info.plist");

extern "C" {
    fn geteuid() -> u32;
}

fn ensure_root(action: &str) -> anyhow::Result<()> {
    if unsafe { geteuid() } != 0 {
        anyhow::bail!("driver {action} writes to /Library/Audio/Plug-Ins/HAL — run it as root: `sudo squeezed driver {action}`");
    }
    Ok(())
}

pub fn install() -> anyhow::Result<()> {
    ensure_root("install")?;

    let macos_dir = Path::new(BUNDLE_PATH).join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", macos_dir.display()))?;
    std::fs::write(
        Path::new(BUNDLE_PATH).join("Contents/Info.plist"),
        INFO_PLIST,
    )?;
    let binary = macos_dir.join("SqueezedAudio");
    std::fs::write(&binary, DRIVER_BINARY)?;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;

    // Re-sign in place (ad hoc). The embedded binary already carries the
    // signature from build time, so a missing codesign tool is not fatal.
    match Command::new("codesign")
        .args(["--force", "--sign", "-", BUNDLE_PATH])
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            "codesign failed (continuing): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("codesign not run (continuing): {e}"),
    }

    restart_coreaudiod()?;
    println!("Installed {BUNDLE_PATH} and restarted coreaudiod.");
    println!("The \"Squeezed\" device should now appear in System Settings → Sound → Output.");
    println!("Select it there, then start the server with: squeezed --source virtual");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    ensure_root("uninstall")?;
    if !Path::new(BUNDLE_PATH).exists() {
        println!("Nothing to do — {BUNDLE_PATH} is not installed.");
        return Ok(());
    }
    std::fs::remove_dir_all(BUNDLE_PATH)
        .map_err(|e| anyhow::anyhow!("removing {BUNDLE_PATH}: {e}"))?;
    restart_coreaudiod()?;
    println!("Removed {BUNDLE_PATH} and restarted coreaudiod.");
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    let installed = Path::new(BUNDLE_PATH).exists();
    println!(
        "driver bundle: {}",
        if installed {
            "installed"
        } else {
            "not installed (run `sudo squeezed driver install`)"
        }
    );
    let visible = crate::capture::find_device()?.is_some();
    println!(
        "audio device:  {}",
        match (installed, visible) {
            (_, true) => "visible to CoreAudio (\"Squeezed\" in Sound settings)",
            (true, false) => "not visible yet — restarting coreaudiod or logging out/in may help",
            (false, false) => "not present",
        }
    );
    Ok(())
}

/// coreaudiod only scans the HAL plug-in directory at startup. SIP forbids
/// `launchctl kickstart` on coreaudiod; killing it works — launchd relaunches
/// it immediately.
fn restart_coreaudiod() -> anyhow::Result<()> {
    let killall = Command::new("killall")
        .arg("coreaudiod")
        .output()
        .map_err(|e| anyhow::anyhow!("restarting coreaudiod: {e}"))?;
    // killall failing usually means coreaudiod wasn't running; launchd will
    // start it on demand either way.
    if !killall.status.success() {
        tracing::warn!(
            "could not restart coreaudiod ({}); it will pick the driver up on next start",
            String::from_utf8_lossy(&killall.stderr).trim()
        );
    }
    Ok(())
}
