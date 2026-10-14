//! The `.zwo` files the home screen offers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::workout::Workout;

/// Copied into an empty workouts folder so there's something to ride.
const EXAMPLES: [(&str, &str); 2] = [
    ("spin_up_40.zwo", include_str!("../workouts/spin_up_40.zwo")),
    ("sweet_spot_3x10.zwo", include_str!("../workouts/sweet_spot_3x10.zwo")),
];

pub struct Entry {
    pub path: PathBuf,
    pub workout: Result<Workout, String>,
}

impl Entry {
    pub fn file_name(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    pub fn title(&self) -> String {
        match &self.workout {
            Ok(w) if !w.name.is_empty() => w.name.clone(),
            _ => self.file_name(),
        }
    }
}

fn zwo_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zwo")) {
            files.push(path);
        }
    }
    Ok(files)
}

/// Workouts in `dir`, sorted by title; files that don't parse are listed with their error.
pub fn scan(dir: &Path) -> Result<Vec<Entry>> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        for (name, xml) in EXAMPLES {
            std::fs::write(dir.join(name), xml)?;
        }
    }
    let mut entries: Vec<Entry> = zwo_files(dir)?
        .into_iter()
        .map(|path| {
            let workout = Workout::load(&path).map_err(|e| format!("{e:#}"));
            Entry { path, workout }
        })
        .collect();
    entries.sort_by_key(|e| e.title().to_lowercase());
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_folder_gets_examples_and_broken_files_are_listed() {
        let dir = std::env::temp_dir().join(format!("watty-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let entries = scan(&dir).unwrap();
        assert_eq!(entries.len(), EXAMPLES.len());
        assert!(entries.iter().all(|e| e.workout.is_ok()));

        std::fs::write(dir.join("broken.zwo"), "<nope").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        let entries = scan(&dir).unwrap();
        assert_eq!(entries.len(), EXAMPLES.len() + 1);
        let broken = entries.iter().find(|e| e.file_name() == "broken.zwo").unwrap();
        assert!(broken.workout.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
