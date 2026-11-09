use std::{fs, io::Read, path::Path};

use anyhow::{Result, anyhow};
use scheduler_core::MAX_FRAME_BYTES;
use scheduler_platform::DataPaths;
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) struct Bridge<'a> {
    pub program: &'a str,
    pub data_dir: Option<&'a Path>,
}

impl<'a> Bridge<'a> {
    pub fn updated(self, desired: Bridge<'a>) -> Self {
        if Path::new(self.program).is_absolute() && Path::new(desired.program).is_absolute() {
            Self {
                program: desired.program,
                ..self
            }
        } else {
            self
        }
    }

    pub fn recognize(
        program: &'a str,
        args: &[&'a str],
        desired: Bridge<'_>,
        host_args: &[&'static str],
        config_file: &Path,
    ) -> Option<Result<Self>> {
        let options = args
            .strip_prefix(&["--project-root", "."])?
            .strip_suffix(host_args)?;
        let data_dir = match options {
            [] => None,
            ["--data-dir", path] if !path.is_empty() => Some(Path::new(*path)),
            _ => return None,
        };
        if program != desired.program && !packaged_bridge(program) {
            return None;
        }
        if same_state(data_dir, desired.data_dir) {
            return Some(Ok(Self { program, data_dir }));
        }
        if data_dir.is_some_and(|path| !path.is_absolute()) {
            return None;
        }
        let configured = state_description(data_dir);
        let active = state_description(desired.data_dir);
        Some(Err(anyhow!(
            "[runspur_state_conflict] {} is configured for the Runspur state directory \"{configured}\". This Runspur instance uses \"{active}\". Open Runspur with the configured --data-dir to edit this project, or choose a separate project folder.",
            config_file.display()
        )))
    }
}

fn state_description(path: Option<&Path>) -> String {
    let default = DataPaths::default_root().ok();
    path.or(default.as_deref()).map_or_else(
        || "default Runspur state directory".into(),
        |path| path.display().to_string(),
    )
}

fn same_state(left: Option<&Path>, right: Option<&Path>) -> bool {
    if left == right {
        return true;
    }
    let default = DataPaths::default_root().ok();
    let (Some(left), Some(right)) = (left.or(default.as_deref()), right.or(default.as_deref()))
    else {
        return false;
    };
    left.is_absolute()
        && right.is_absolute()
        && (left == right
            || fs::canonicalize(left)
                .ok()
                .zip(fs::canonicalize(right).ok())
                .is_some_and(|(left, right)| left == right))
}

fn packaged_bridge(program: &str) -> bool {
    if program == "scheduler-mcp" {
        return true;
    }
    let path = Path::new(program);
    if !path.is_absolute() {
        return false;
    }
    #[cfg(windows)]
    if default_install_bridge(path) {
        return true;
    }
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    let suffix = std::env::consts::EXE_SUFFIX;
    let filename = format!("scheduler-mcp{suffix}");
    let Some(bundle) = path.parent().filter(|_| {
        path.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(&filename))
    }) else {
        return false;
    };
    let cli = format!("runspur{suffix}");
    let control = format!("schedulerctl{suffix}");
    (bundle.join(&cli).is_file() && npm_bundle(bundle, &cli))
        || (bundle.join(&control).is_file()
            && bundle.join(format!("schedulerd{suffix}")).is_file()
            && (bundle.join(format!("runspur-desktop{suffix}")).is_file()
                || bundle.join(".runspur-install.json").is_file()))
}

#[cfg(windows)]
fn default_install_bridge(path: &Path) -> bool {
    // An uninstall can remove the siblings that identify an installed package.
    let Some(path) = path.to_str() else {
        return false;
    };
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path = path.replace('/', "\\");
    [
        ("LOCALAPPDATA", r"Runspur Desktop\scheduler-mcp.exe"),
        (
            "LOCALAPPDATA",
            r"Programs\Runspur Desktop\scheduler-mcp.exe",
        ),
        ("ProgramFiles", r"Runspur Desktop\scheduler-mcp.exe"),
        (
            "APPDATA",
            r"npm\node_modules\runspur\native\scheduler-mcp.exe",
        ),
    ]
    .into_iter()
    .any(|(variable, relative)| {
        std::env::var_os(variable)
            .map(|root| Path::new(&root).join(relative))
            .is_some_and(|installed| installed.as_os_str().eq_ignore_ascii_case(&path))
    })
}

fn npm_bundle(bundle: &Path, control: &str) -> bool {
    if bundle.file_name().is_none_or(|name| name != "native") {
        return false;
    }
    let Some(root) = bundle.parent() else {
        return false;
    };
    let Ok(file) = fs::File::open(root.join("package.json")) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_FRAME_BYTES
    {
        return false;
    }
    let Ok(manifest) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    manifest["name"] == "runspur" && manifest["bin"]["runspur"] == format!("native/{control}")
}
