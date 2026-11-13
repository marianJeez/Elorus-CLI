//! Third-party provider keys, sealed at rest in ~/.elorus/provider_keys.json. One file for all
//! of them, so setting or forgetting one never rewrites another provider's config.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::paths::Paths;
use crate::io;
use crate::io::fs::Access;

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default = "io::secret::version")]
    v: u32,
    #[serde(default)]
    keys: BTreeMap<String, Sealed>,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    scheme: String,
    secret: String,
}

fn unreadable(paths: &Paths) -> String {
    format!(
        "the provider keys in {} are unreadable; set the key again",
        paths.provider_keys().display()
    )
}

fn read(paths: &Paths) -> anyhow::Result<Option<Store>> {
    let path = paths.provider_keys();
    let Some(text) = io::fs::read_string(&path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| unreadable(paths))
}

/// The key stored for `id`, if any.
pub fn get(paths: &Paths, id: &str) -> anyhow::Result<Option<String>> {
    let Some(store) = read(paths)? else {
        return Ok(None);
    };
    let Some(sealed) = store.keys.get(id) else {
        return Ok(None);
    };
    let bytes = io::secret::open_b64(&sealed.scheme, &sealed.secret)
        .with_context(|| format!("{}: set it again", unreadable(paths)))?;
    let key = String::from_utf8(bytes).with_context(|| unreadable(paths))?;
    Ok(Some(key))
}

pub fn has(paths: &Paths, id: &str) -> anyhow::Result<bool> {
    Ok(read(paths)?.is_some_and(|store| store.keys.contains_key(id)))
}

pub fn set(paths: &Paths, id: &str, key: &str) -> anyhow::Result<()> {
    let (scheme, secret) = io::secret::seal_b64(key.as_bytes())?;
    let mut store = read(paths)?.unwrap_or_default();
    store.keys.insert(
        id.to_owned(),
        Sealed {
            scheme: scheme.to_owned(),
            secret,
        },
    );
    store.v = io::secret::version();
    io::fs::write_json(&paths.provider_keys(), &store, Access::Private)?;
    Ok(())
}

/// `Ok(false)` when there was no key to forget.
pub fn remove(paths: &Paths, id: &str) -> anyhow::Result<bool> {
    let Some(mut store) = read(paths)? else {
        return Ok(false);
    };
    let removed = store.keys.remove(id).is_some();
    if removed {
        io::fs::write_json(&paths.provider_keys(), &store, Access::Private)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_are_sealed() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path().into(), home.path().into());
        assert_eq!(get(&paths, "p").unwrap(), None);
        set(&paths, "p", "sk-secret").unwrap();
        set(&paths, "q", "other").unwrap();
        assert_eq!(get(&paths, "p").unwrap().as_deref(), Some("sk-secret"));
        let text = io::fs::read_string(&paths.provider_keys())
            .unwrap()
            .unwrap();
        assert!(!text.contains("sk-secret") || cfg!(not(windows)));
        assert!(remove(&paths, "p").unwrap());
        assert!(!remove(&paths, "p").unwrap());
        assert_eq!(get(&paths, "q").unwrap().as_deref(), Some("other"));
    }

    #[test]
    fn a_corrupt_store_says_so_rather_than_looking_empty() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path().into(), home.path().into());
        io::fs::write_atomic(&paths.provider_keys(), b"{", Access::Private).unwrap();
        let err = format!("{:#}", get(&paths, "p").err().unwrap());
        assert!(err.contains("provider_keys.json"), "{err}");
    }
}
