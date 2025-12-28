use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub name: String,
    pub peer: SocketAddr,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct ProfileFile {
    profiles: Vec<ConnectionProfile>,
}

pub struct ProfileStore {
    path: PathBuf,
    data: ProfileFile,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            path: profile_path(),
            data: ProfileFile::default(),
        }
    }
}

impl ProfileStore {
    pub fn load() -> io::Result<Self> {
        let path = profile_path();
        let data = match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => ProfileFile::default(),
            Err(err) => return Err(err),
        };

        Ok(Self { path, data })
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            ensure_dir(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.data)?;
        fs::write(&self.path, json)
    }

    pub fn upsert(&mut self, profile: ConnectionProfile) {
        if let Some(existing) = self
            .data
            .profiles
            .iter_mut()
            .find(|p| p.name == profile.name)
        {
            *existing = profile;
        } else {
            self.data.profiles.push(profile);
            self.data
                .profiles
                .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.data.profiles.len();
        self.data.profiles.retain(|p| p.name != name);
        before != self.data.profiles.len()
    }

    pub fn find(&self, name: &str) -> Option<ConnectionProfile> {
        self.data.profiles.iter().find(|p| p.name == name).cloned()
    }

    pub fn profiles(&self) -> &[ConnectionProfile] {
        &self.data.profiles
    }
}

fn profile_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".pasta_p2p"));

    base.join("pasta_p2p").join("profiles.json")
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path)
}

pub fn print_profiles(store: &ProfileStore) {
    if store.profiles().is_empty() {
        println!("nenhum perfil salvo");
        return;
    }

    println!("perfis salvos:");
    for profile in store.profiles() {
        let alias = profile
            .alias
            .as_deref()
            .map(|alias| format!(" ({alias})"))
            .unwrap_or_default();
        let key_hint = if profile.key.is_some() {
            " • chave armazenada"
        } else {
            ""
        };
        println!(
            "- {}{} -> {}{}",
            profile.name, alias, profile.peer, key_hint
        );
    }
}
