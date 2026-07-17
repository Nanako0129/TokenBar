use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

const CACHE_TTL_SECS: u64 = 3600;

pub fn get_cache_dir() -> PathBuf {
    crate::paths::get_cache_dir()
}

pub fn get_cache_path(filename: &str) -> PathBuf {
    get_cache_dir().join(filename)
}

#[derive(Serialize, Deserialize)]
pub struct CachedData<T> {
    pub timestamp: u64,
    pub data: T,
}

fn load_cache_with_policy<T: for<'de> Deserialize<'de>>(
    filename: &str,
    allow_stale: bool,
) -> Option<T> {
    let canonical_path = get_cache_path(filename);
    let cached: CachedData<T> = match fs::read_to_string(&canonical_path) {
        Ok(content) => serde_json::from_str(&content).ok()?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            legacy_cache_paths(filename).into_iter().find_map(|path| {
                let content = fs::read_to_string(&path).ok()?;
                serde_json::from_str(&content).ok()
            })?
        }
        Err(_) => return None,
    };

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();

    if cached.timestamp > now {
        return None;
    }

    if !allow_stale && now.saturating_sub(cached.timestamp) > CACHE_TTL_SECS {
        return None;
    }

    Some(cached.data)
}

pub fn load_cache<T: for<'de> Deserialize<'de>>(filename: &str) -> Option<T> {
    load_cache_with_policy(filename, false)
}

pub fn load_cache_any_age<T: for<'de> Deserialize<'de>>(filename: &str) -> Option<T> {
    load_cache_with_policy(filename, true)
}

/// Unix-seconds timestamp recorded when this cache file was last written (i.e.
/// when its data was last fetched), regardless of staleness. `None` when the
/// file is absent or unreadable. The payload is parsed as opaque JSON so this
/// stays cheap and type-agnostic.
pub fn cache_timestamp(filename: &str) -> Option<u64> {
    let canonical_path = get_cache_path(filename);
    let content = match fs::read_to_string(&canonical_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => legacy_cache_paths(filename)
            .into_iter()
            .find_map(|path| fs::read_to_string(&path).ok())?,
        Err(_) => return None,
    };
    let parsed: CachedData<serde_json::Value> = serde_json::from_str(&content).ok()?;
    Some(parsed.timestamp)
}

pub fn save_cache<T: Serialize>(filename: &str, data: &T) -> Result<(), std::io::Error> {
    let dir = get_cache_dir();
    fs::create_dir_all(&dir)?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();

    let cached = CachedData {
        timestamp: now,
        data,
    };
    let content = serde_json::to_string(&cached)?;

    let final_path = get_cache_path(filename);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let tmp_filename = format!(".{}.{}.{:x}.tmp", filename, std::process::id(), nanos);
    let tmp_path = dir.join(&tmp_filename);

    use std::io::Write;
    // INVARIANT: All cache writes use atomic temp-file rename. NEVER delete
    // the canonical cache file before writing — a partial save or process
    // crash between delete and rename would lose the cache. The temp-file
    // pattern makes corruption-on-crash impossible.
    let write_result = (|| {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        crate::fs_atomic::replace_file(&tmp_path, &final_path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    write_result
}

fn legacy_cache_paths(filename: &str) -> Vec<PathBuf> {
    if crate::paths::is_config_dir_overridden() {
        return Vec::new();
    }

    [
        crate::paths::legacy_dirs_cache_dir().map(|d| d.join(filename)),
        crate::paths::legacy_dot_cache_tokscale_dir().map(|d| d.join(filename)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    #[cfg(not(target_os = "windows"))]
    use tempfile::TempDir;

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|key| (*key, env::var_os(key))).collect())
        }

        fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
            unsafe { env::set_var(key, value) };
        }

        fn remove(&mut self, key: &'static str) {
            unsafe { env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => env::set_var(key, value),
                        None => env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    #[serial]
    #[cfg(not(target_os = "windows"))]
    fn load_falls_back_to_legacy_dirs_cache_path() {
        let temp_home = TempDir::new().unwrap();
        let temp_xdg_cache = TempDir::new().unwrap();
        let config_dir = temp_home.path().join(".config");
        let mut _env = EnvGuard::capture(&[
            "HOME",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "TOKSCALE_CONFIG_DIR",
        ]);
        _env.set("HOME", temp_home.path());
        _env.set("XDG_CACHE_HOME", temp_xdg_cache.path());
        // Pin XDG_CONFIG_HOME so paths::get_cache_dir() stays inside
        // the sandboxed HOME on Linux CI runners that set this var globally.
        _env.set("XDG_CONFIG_HOME", &config_dir);
        _env.remove("TOKSCALE_CONFIG_DIR");

        let legacy_path = crate::paths::legacy_dirs_cache_dir()
            .unwrap()
            .join("pricing-litellm.json");
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::write(
            &legacy_path,
            format!(r#"{{"timestamp":{now},"data":{{"ok":true}}}}"#),
        )
        .unwrap();

        let loaded: Option<serde_json::Value> = load_cache("pricing-litellm.json");
        assert_eq!(loaded.unwrap()["ok"], serde_json::json!(true));
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn legacy_cache_paths_are_ordered_and_override_gated_without_io() {
        let mut _env = EnvGuard::capture(&[
            "HOME",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "TOKSCALE_CONFIG_DIR",
        ]);
        _env.remove("TOKSCALE_CONFIG_DIR");
        let candidates = legacy_cache_paths("pricing-litellm.json");
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0],
            dirs::cache_dir()
                .expect("Windows exposes a cache directory")
                .join("tokscale")
                .join("pricing-litellm.json")
        );
        assert_eq!(
            candidates[1],
            dirs::home_dir()
                .expect("Windows exposes a home directory")
                .join(".cache")
                .join("tokscale")
                .join("pricing-litellm.json")
        );

        _env.set("TOKSCALE_CONFIG_DIR", std::env::temp_dir());
        assert!(legacy_cache_paths("pricing-litellm.json").is_empty());
    }

    #[test]
    #[serial]
    fn env_guard_restores_after_unwind() {
        const KEY: &str = "TOKSCALE_PRICING_CACHE_ENV_GUARD_SELF_CHECK";
        let mut outer = EnvGuard::capture(&[KEY]);
        outer.set(KEY, "before");
        let result = std::panic::catch_unwind(|| {
            let mut inner = EnvGuard::capture(&[KEY]);
            inner.set(KEY, "during");
            panic!("exercise EnvGuard unwinding");
        });
        assert!(result.is_err());
        assert_eq!(env::var_os(KEY), Some("before".into()));
    }
}
