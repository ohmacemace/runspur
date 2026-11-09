use std::{collections::HashSet, path::Path};

use scheduler_core::ProcessIdentity;
use scheduler_platform::{current_principal, is_running, process_info, process_principal};

const MAX_ANCESTRY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHost {
    DesktopProcess,
    LocalCli,
    Unknown,
}

struct HostPaths {
    local_app_data: String,
    roaming_app_data: String,
    program_files: String,
    system_root: String,
}

impl HostPaths {
    fn read() -> Option<Self> {
        let directory = |name| {
            std::env::var_os(name).map(|path| {
                normalized_image(Path::new(&path))
                    .trim_end_matches('/')
                    .to_owned()
            })
        };
        Some(Self {
            local_app_data: directory("LOCALAPPDATA")?,
            roaming_app_data: directory("APPDATA")?,
            program_files: directory("ProgramFiles")?,
            system_root: directory("SystemRoot")?,
        })
    }

    fn packaged_image<'a>(&self, image: &'a str) -> Option<&'a str> {
        let relative =
            image.strip_prefix(&format!("{}/windowsapps/openai.codex_", self.program_files))?;
        let (package, entry) = relative.split_once("/app/")?;
        (!package.contains('/') && package.ends_with("__2p2nqsd0c76g0")).then_some(entry)
    }

    fn cached_core(&self, image: &str) -> bool {
        image
            .strip_prefix(&format!("{}/openai/codex/bin/", self.local_app_data))
            .and_then(|relative| relative.split_once('/'))
            .is_some_and(|(cache_key, file)| !cache_key.is_empty() && file == "codex.exe")
    }

    fn npm_core(&self, image: &str) -> bool {
        let Some(relative) = image.strip_prefix(&format!(
            "{}/npm/node_modules/@openai/",
            self.roaming_app_data
        )) else {
            return false;
        };
        let package = relative
            .strip_prefix("codex/node_modules/@openai/")
            .unwrap_or(relative);
        ["codex-win32-x64", "codex-win32-arm64"]
            .into_iter()
            .any(|name| {
                package.strip_prefix(name).is_some_and(|relative| {
                    matches!(
                        relative,
                        "/vendor/x86_64-pc-windows-msvc/bin/codex.exe"
                            | "/vendor/aarch64-pc-windows-msvc/bin/codex.exe"
                    )
                })
            })
    }

    fn local_shell(&self, image: &str) -> bool {
        image
            .strip_prefix(&self.system_root)
            .is_some_and(|relative| {
                matches!(
                    relative,
                    "/system32/cmd.exe" | "/system32/windowspowershell/v1.0/powershell.exe"
                )
            })
            || image == format!("{}/powershell/7/pwsh.exe", self.program_files)
    }
}

/// Classifies a known local launcher from the operating system's pipe peer.
/// DesktopProcess is evidence of the launcher only; the desktop adapter must
/// still prove that the exact thread has a verified owner. LocalCli is
/// diagnostic and does not establish a deliverable conversation.
pub fn classify_codex_host(peer: &ProcessIdentity) -> CodexHost {
    let (Some(paths), Ok(principal)) = (HostPaths::read(), current_principal()) else {
        return CodexHost::Unknown;
    };
    let mut images = Vec::new();
    let mut seen = HashSet::new();
    let mut next_pid = Some(peer.pid);
    let mut child_started_at = peer.started_at;
    for _ in 0..MAX_ANCESTRY {
        let Some(pid) = next_pid.filter(|pid| seen.insert(*pid)) else {
            break;
        };
        let Ok(process) = process_info(pid) else {
            break;
        };
        if (images.is_empty() && process.identity != *peer)
            || process.identity.started_at > child_started_at
            || process_principal(pid).ok().as_ref() != Some(&principal)
            || !is_running(&process.identity)
        {
            break;
        }
        images.push(normalized_image(&process.executable));
        child_started_at = process.identity.started_at;
        next_pid = process.parent_pid;
    }
    classify_images(&images, &paths)
}

fn normalized_image(path: &Path) -> String {
    let path = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    path.strip_prefix("//?/").unwrap_or(&path).to_owned()
}

fn classify_images(images: &[String], paths: &HostPaths) -> CodexHost {
    let [bridge, core, launchers @ ..] = images else {
        return CodexHost::Unknown;
    };
    let npm_core = paths.npm_core(core);
    if !bridge.ends_with("/scheduler-mcp.exe")
        || !(npm_core
            || paths.cached_core(core)
            || paths.packaged_image(core) == Some("resources/codex.exe"))
    {
        return CodexHost::Unknown;
    }
    let is_desktop = |image: &String| {
        matches!(
            paths.packaged_image(image),
            Some("chatgpt.exe" | "codex.exe")
        )
    };
    if launchers.first().is_some_and(is_desktop) {
        return CodexHost::DesktopProcess;
    }
    // A terminal opened within Desktop can launch a separate local Codex process.
    if launchers.iter().skip(1).any(is_desktop) {
        return CodexHost::Unknown;
    }
    match launchers {
        [shell, ..] if paths.local_shell(shell) => CodexHost::LocalCli,
        [node, shell, ..]
            if npm_core
                && *node == format!("{}/nodejs/node.exe", paths.program_files)
                && paths.local_shell(shell) =>
        {
            CodexHost::LocalCli
        }
        _ => CodexHost::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIDGE: &str = "d:/runspur/target/debug/scheduler-mcp.exe";
    const CACHED_CORE: &str = "c:/users/test/appdata/local/openai/codex/bin/cache/codex.exe";
    const DESKTOP: &str = "c:/program files/windowsapps/openai.codex_26.1103.4239.0_x64__2p2nqsd0c76g0/app/chatgpt.exe";
    const NPM_CORE: &str = "c:/users/test/appdata/roaming/npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe";
    const POWERSHELL: &str = "c:/windows/system32/windowspowershell/v1.0/powershell.exe";

    fn classify(images: &[&str]) -> CodexHost {
        classify_images(
            &images
                .iter()
                .map(|image| (*image).to_owned())
                .collect::<Vec<_>>(),
            &HostPaths {
                local_app_data: "c:/users/test/appdata/local".into(),
                roaming_app_data: "c:/users/test/appdata/roaming".into(),
                program_files: "c:/program files".into(),
                system_root: "c:/windows".into(),
            },
        )
    }

    #[test]
    fn the_same_codex_core_is_distinguished_by_its_launcher() {
        assert_eq!(
            classify(&[BRIDGE, CACHED_CORE, DESKTOP]),
            CodexHost::DesktopProcess
        );
        assert_eq!(
            classify(&[BRIDGE, CACHED_CORE, POWERSHELL]),
            CodexHost::LocalCli
        );
    }

    #[test]
    fn npm_cli_has_a_known_native_image_and_local_shell() {
        let node = "c:/program files/nodejs/node.exe";
        assert_eq!(
            classify(&[BRIDGE, NPM_CORE, node, POWERSHELL]),
            CodexHost::LocalCli
        );
    }

    #[test]
    fn a_shell_inside_desktop_does_not_prove_desktop_ownership() {
        assert_eq!(
            classify(&[BRIDGE, CACHED_CORE, POWERSHELL, DESKTOP]),
            CodexHost::Unknown
        );
    }

    #[test]
    fn names_without_verified_ancestry_are_inconclusive() {
        let arbitrary_codex = "d:/project/codex.exe";
        assert_eq!(
            classify(&[BRIDGE, arbitrary_codex, POWERSHELL]),
            CodexHost::Unknown
        );
        assert_eq!(classify(&[BRIDGE, CACHED_CORE]), CodexHost::Unknown);
        assert_eq!(
            classify(&[POWERSHELL, NPM_CORE, POWERSHELL]),
            CodexHost::Unknown
        );
    }
}
