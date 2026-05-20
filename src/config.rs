use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

pub const LSL_DIR: &str = "/var/lib/lsl";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DistroConfig {
    pub name: String,
    pub path: String,
    pub ip_address: String,
    pub mac_address: String,
    pub default_user: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct GlobalConfig {
    pub default_distro: Option<String>,
    pub distros: HashMap<String, DistroConfig>,
}

impl GlobalConfig {
    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.exists() {
            return Self::default();
        }
        File::open(path)
            .and_then(|file| serde_json::from_reader(file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)))
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, self)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn config_path() -> PathBuf {
        Path::new(LSL_DIR).join("config.json")
    }
}

pub fn ensure_dirs() -> io::Result<()> {
    let lsl_path = Path::new(LSL_DIR);
    fs::create_dir_all(lsl_path.join("distros"))?;
    fs::create_dir_all(lsl_path.join("cache"))?;
    // Create subdirs inside run directory for PID storage
    fs::create_dir_all(lsl_path.join("run"))?;
    Ok(())
}

pub fn get_distro_dir(name: &str) -> PathBuf {
    Path::new(LSL_DIR).join("distros").join(name)
}
