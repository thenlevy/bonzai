//! What `tests/fixtures/make-db.sh` leaves in `postgres/`: the cluster, the
//! facts each fixture script printed, and the files it wrote.

use std::{collections::HashMap, fmt::Debug, fs, str::FromStr};

/// The cluster `make-db.sh` builds, and the facts it saves next to it.
pub const POSTGRES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/postgres");

/// What a fixture script printed: one `name=value` line per value.
pub struct Facts(HashMap<String, String>);

impl Facts {
    pub fn read(fixture: &str) -> Self {
        let text = String::from_utf8(read(&format!("{fixture}.facts"))).unwrap();
        let values = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Facts(values)
    }

    pub fn get<T: FromStr<Err: Debug>>(&self, name: &str) -> T {
        let value = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("no {name} in the facts"));
        value.parse().unwrap()
    }
}

/// Reads a file under `postgres/`. Panics if it isn't there.
pub fn read(path: &str) -> Vec<u8> {
    let path = format!("{POSTGRES_DIR}/{path}");
    fs::read(&path).unwrap_or_else(|err| {
        panic!("could not read {path}: {err}. Build the cluster with `tests/fixtures/make-db.sh`.")
    })
}
