use std::{fs, io::Read, path::Path};

use scheduler_core::MAX_FRAME_BYTES;
use scheduler_platform::DataPaths;
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) struct Bridge<'a> {
    pub program: &'a str,
    pub data_dir: Option<&'a Path>,
}

impl<'a> Bridge<'a> {
    pub fn recognize(
        program: &'a str,
        args: &[&'a str],
        desired: Bridge<'_>,
        host_args: &[&'static str],
    ) -> Option<Self> {
        let options = args
            .strip_prefix(&["--project-root", "."])?
            .strip_suffix(host_args)?;
        let data_dir = match options {
            [] => None,
            ["--data-dir", path] if !path.is_empty() => Some(Path::new(*path)),
            _ => return None,
        };
        (same_state(data_dir, desired.data_dir)
            && (program == desired.program || packaged_bridge(program)))
        .then_some(Self { program, data_dir })
    }
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
    let control = format!("schedulerctl{suffix}");
    if !bundle.join(&control).is_file() {
        return false;
    }
    npm_bundle(bundle, &control)
        || (bundle.join(format!("schedulerd{suffix}")).is_file()
            && (bundle.join(format!("runspur-desktop{suffix}")).is_file()
                || bundle.join(".runspur-install.json").is_file()))
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
