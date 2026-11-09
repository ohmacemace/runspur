use std::{fs, io::ErrorKind, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Preferences {
    #[serde(default)]
    pub language: Language,
}

pub fn load(path: &Path) -> Result<Preferences> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Cannot read Runspur preferences"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error).context("Cannot open Runspur preferences"),
    }
}

pub fn save(path: &Path, preferences: Preferences) -> Result<()> {
    let directory = path
        .parent()
        .context("Cannot locate the preferences directory")?;
    fs::create_dir_all(directory)?;
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut pending, &preferences)?;
    pending.as_file().sync_all()?;
    pending
        .persist(path)
        .context("Cannot save Runspur preferences")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_preferences_default_to_system_language() {
        let preferences: Preferences =
            serde_json::from_str(r#"{"start_scheduler_on_launch":false}"#).unwrap();
        let saved = serde_json::to_value(preferences).unwrap();
        assert_eq!(saved, serde_json::json!({"language":"system"}));
    }

    #[test]
    fn legacy_settings_preserve_language_without_the_startup_flag() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        assert_eq!(load(&path).unwrap().language, Language::System);
        fs::write(
            &path,
            br#"{"language":"zh-CN","start_scheduler_on_launch":false}"#,
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().language, Language::SimplifiedChinese);

        let preferences = Preferences {
            language: Language::English,
        };
        save(&path, preferences).unwrap();
        assert_eq!(load(&path).unwrap(), preferences);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted, serde_json::json!({"language":"en"}));
    }
}
