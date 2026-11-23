//! micd's files in `~/.config/mic-dsp`: the config it reloads while running and the lock that
//! keeps a second micd out. Control thread only.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dsp::{Chain, Config, Control};

/// `~/.config/mic-dsp`, created if needed.
pub fn dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let dir = Path::new(&home).join(".config/mic-dsp");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Holds an exclusive `flock` on `micd.lock` for as long as it lives.
pub struct Lock {
    _file: File,
}

/// `Ok(None)` when another process holds the lock.
pub fn lock(dir: &Path) -> io::Result<Option<Lock>> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("micd.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(Lock { _file: file }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(error)
    }
}

/// What identifies one version of a file without reading it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: (i64, i64),
    len: u64,
    inode: u64,
}

impl Stamp {
    /// `None` when the file is missing.
    fn of(path: &Path) -> io::Result<Option<Stamp>> {
        match fs::metadata(path) {
            Ok(meta) => Ok(Some(Stamp {
                modified: (meta.mtime(), meta.mtime_nsec()),
                len: meta.len(),
                inode: meta.ino(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// The config and where it lives.
pub struct Settings {
    config_path: PathBuf,
    /// The version of `config.toml` last looked at. The outer `None` means never.
    config_stamp: Option<Option<Stamp>>,
    config: Config,
    /// The last failure to read the config's metadata, logged once.
    error: Option<String>,
}

impl Settings {
    pub fn new(dir: &Path) -> Settings {
        Settings {
            config_path: dir.join("config.toml"),
            config_stamp: None,
            config: Config::default(),
            error: None,
        }
    }

    /// A chain for a new engine, with the current config.
    pub fn chain(&mut self) -> (Chain, Control) {
        self.reload_config();
        dsp::chain(&self.config)
    }

    /// The periodic work while a chain runs: applies a changed config.
    pub fn tick(&mut self, control: &mut Control) {
        if self.reload_config() {
            control.set_config(&self.config);
        }
    }

    /// Reads `config.toml` again if it changed since the last look. Returns whether the config
    /// changed. A missing file means the defaults; a broken one keeps the previous config.
    fn reload_config(&mut self) -> bool {
        let stamp = match Stamp::of(&self.config_path) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.report(format!(
                    "cannot read {}: {error}",
                    self.config_path.display()
                ));
                return false;
            }
        };
        if self.config_stamp == Some(stamp) {
            return false;
        }
        self.config_stamp = Some(stamp);
        let path = self.config_path.display();
        let loaded = if stamp.is_none() {
            log!("no {path}; using the default config");
            Config::default()
        } else {
            let parsed = fs::read_to_string(&self.config_path)
                .map_err(|error| error.to_string())
                .and_then(|text| Config::from_toml(&text).map_err(|error| error.to_string()));
            match parsed {
                Ok(config) => {
                    log!("loaded {path}");
                    config
                }
                Err(error) => {
                    // toml's messages span lines; the log keeps one event per line.
                    let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
                    log!("{path}: {error}; keeping the previous config");
                    return false;
                }
            }
        };
        let changed = loaded != self.config;
        self.config = loaded;
        changed
    }

    fn report(&mut self, error: String) {
        if self.error.as_ref() != Some(&error) {
            log!("{error}");
            self.error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("micd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_config_reloads_only_when_it_changes_and_never_into_a_broken_one() {
        let dir = scratch("config");
        let mut settings = Settings::new(&dir);
        drop(settings.chain());
        assert_eq!(settings.config, Config::default());
        assert!(
            !dir.join("config.toml").exists(),
            "a missing config is not created"
        );

        let path = dir.join("config.toml");
        fs::write(&path, "[target]\npeak_dbfs = -14.0\n").unwrap();
        assert!(settings.reload_config());
        let quieter = Config::from_toml("[target]\npeak_dbfs = -14.0").unwrap();
        assert_eq!(settings.config, quieter);
        assert!(
            !settings.reload_config(),
            "an unchanged file is not read again"
        );

        fs::write(&path, "[target]\npeak_dbfs = 3.0\n").unwrap();
        assert!(!settings.reload_config());
        assert_eq!(
            settings.config, quieter,
            "an invalid value keeps the previous config"
        );
        fs::write(&path, "not toml at all").unwrap();
        assert!(!settings.reload_config());
        assert_eq!(settings.config, quieter);

        fs::remove_file(&path).unwrap();
        assert!(settings.reload_config());
        assert_eq!(settings.config, Config::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_state_file_is_left_alone() {
        let dir = scratch("state");
        let path = dir.join("state.toml");
        let old = "version = 1\nspeech_p95_dbfs = -18.0\nspread_db = 7.0\nfloor_dbfs = -67.0\n";
        fs::write(&path, old).unwrap();
        let mut settings = Settings::new(&dir);
        let (chain, mut control) = settings.chain();
        settings.tick(&mut control);
        assert_eq!(control.status().gain_db, 4.5);
        drop((chain, control));
        assert_eq!(fs::read_to_string(&path).unwrap(), old);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_released() {
        let dir = scratch("lock");
        let first = lock(&dir).unwrap();
        assert!(first.is_some());
        // flock locks belong to the open file, so a second open in the same process conflicts.
        assert!(lock(&dir).unwrap().is_none());
        drop(first);
        assert!(lock(&dir).unwrap().is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
