//! Random operations on an undamaged volume, each checked against a model of
//! what the volume should hold, with the whole tree compared at the end and
//! the volume checked by fsck_msdos -n. This is what finds a write that lands
//! in the wrong place, an entry placed over another, or a chain linked wrong,
//! none of which a single scripted test is sure to reach.

mod common;

use common::*;
use fatdisk::damage::Random;
use fatdisk::fat::{join, name, FsError};
use fatdisk::macos;
use std::collections::BTreeMap;

type Model = BTreeMap<String, Option<Vec<u8>>>;

fn parent_of(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// The name, as the model holds it, of the entry `wanted` names in `dir`.
fn find(model: &Model, dir: &str, wanted: &str) -> Option<String> {
    model.keys().find(|path| {
        let (parent, child) = parent_of(path);
        parent == dir && name::same(child, wanted)
    }).map(|path| parent_of(path).1.to_string())
}

fn is_under(path: &str, ancestor: &str) -> bool {
    ancestor.is_empty() || path.starts_with(&format!("{}/", ancestor))
}

fn children(model: &Model, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = model.keys().filter(|path| parent_of(path).0 == dir && !path.is_empty()).map(|path| parent_of(path).1.to_string()).collect();
    names.sort();
    names
}

/// Move `from` and everything under it to `to`.
fn rekey(model: &mut Model, from: &str, to: &str) {
    let moved: Vec<String> = model.keys().filter(|path| path.as_str() == from || path.starts_with(&format!("{}/", from))).cloned().collect();
    let mut entries = Vec::new();
    for path in moved {
        let value = model.remove(&path).unwrap();
        entries.push((format!("{}{}", to, &path[from.len()..]), value));
    }
    model.extend(entries);
}

const POOL: [&str; 26] = [
    "a", "A", "b.txt", "B.TXT", "readme.txt", "README.TXT", "Long file name number 1.html", "long FILE name number 1.HTML", "日本語", "日本語.txt", "🎉", "🎉.txt",
    "a name with a trailing period.", "sub", "Sub", "dir", "𠮷", "Mixed Case.Txt", "...", "a:b", " ", "trailing space ", "a sixty-byte name xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "a name of two hundred units yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
    "e.", "notes",
];

fn pick<'a, T>(random: &mut Random, items: &'a [T]) -> &'a T {
    &items[random.below(items.len() as u64) as usize]
}

fn run_seed(seed: u64, steps: usize, with_mac: bool) {
    let dir = scratch(&format!("model-{}", seed));
    let (path, mut volume) = fresh(&dir, 40, None);
    let c = volume.layout().cluster_bytes as u64;
    let mut model: Model = BTreeMap::new();
    let mut random = Random::new(seed);
    for step in 0..steps {
        let dirs: Vec<String> = std::iter::once(String::new()).chain(model.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| k.clone())).collect();
        let files: Vec<String> = model.iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k.clone()).collect();
        let at = pick(&mut random, &dirs).clone();
        let wanted = *pick(&mut random, &POOL);
        let context = format!("seed {} step {}", seed, step);
        match random.below(11) {
            0 | 1 => {
                let expected = match name::check(wanted) {
                    Err(e) => Err(e),
                    Ok(key) if find(&model, &at, key).is_some() => Err(FsError::Exists),
                    Ok(key) => Ok(key.to_string()),
                };
                let make_dir = random.below(3) == 0;
                let got = if make_dir { volume.mkdir(&at, wanted) } else { volume.create(&at, wanted) }.map(|entry| entry.name);
                assert_eq!(got, expected, "{}: {} {:?} in {:?}", context, if make_dir { "mkdir" } else { "create" }, wanted, at);
                if let Ok(key) = expected {
                    model.insert(join(&at, &key), if make_dir { None } else { Some(Vec::new()) });
                }
            }
            2 | 3 if !files.is_empty() => {
                let file = pick(&mut random, &files).clone();
                let len = model[&file].as_ref().unwrap().len() as u64;
                let offset = random.below(len + 3 * c);
                let data: Vec<u8> = (0..random.below(4 * c)).map(|_| random.next() as u8).collect();
                let got = volume.write(&file, offset, &data);
                assert_eq!(got, Ok(data.len()), "{}: write {} bytes at {} of {:?}", context, data.len(), offset, file);
                if !data.is_empty() {
                    let content = model.get_mut(&file).unwrap().as_mut().unwrap();
                    let end = offset as usize + data.len();
                    if content.len() < end {
                        content.resize(end, 0);
                    }
                    content[offset as usize..end].copy_from_slice(&data);
                }
            }
            4 if !files.is_empty() => {
                let file = pick(&mut random, &files).clone();
                let len = model[&file].as_ref().unwrap().len() as u64;
                let new_len = random.below(len + 3 * c);
                assert_eq!(volume.truncate(&file, new_len), Ok(()), "{}: truncate {:?} to {}", context, file, new_len);
                model.get_mut(&file).unwrap().as_mut().unwrap().resize(new_len as usize, 0);
            }
            5 => {
                let want_dir = random.below(2) == 0;
                let expected = match find(&model, &at, wanted) {
                    None => Err(FsError::NotFound),
                    Some(child) => {
                        let target = join(&at, &child);
                        let is_dir = model[&target].is_none();
                        if is_dir != want_dir {
                            Err(if is_dir { FsError::IsDir } else { FsError::NotDir })
                        } else if is_dir && !children(&model, &target).is_empty() {
                            Err(FsError::NotEmpty)
                        } else {
                            Ok(target)
                        }
                    }
                };
                let got = volume.remove(&at, wanted, want_dir);
                assert_eq!(got, expected.clone().map(|_| ()), "{}: remove {:?} in {:?}", context, wanted, at);
                if let Ok(target) = expected {
                    model.remove(&target);
                }
            }
            6 | 7 => {
                let entries: Vec<String> = model.keys().cloned().collect();
                if entries.is_empty() {
                    continue;
                }
                let source = pick(&mut random, &entries).clone();
                let (from_dir, from_name) = parent_of(&source);
                let (from_dir, from_name) = (from_dir.to_string(), from_name.to_string());
                let to_dir = at.clone();
                let expected: Result<(String, Option<String>), FsError> = (|| {
                    let key = name::check(wanted)?;
                    let source_dir = model[&source].is_none();
                    if source_dir && (to_dir == source || is_under(&to_dir, &source)) {
                        return Err(FsError::Invalid);
                    }
                    match find(&model, &to_dir, key) {
                        Some(existing) if join(&to_dir, &existing) == source => Ok((key.to_string(), None)),
                        Some(existing) => {
                            let target = join(&to_dir, &existing);
                            let target_dir = model[&target].is_none();
                            if source_dir && !target_dir {
                                return Err(FsError::NotDir);
                            }
                            if !source_dir && target_dir {
                                return Err(FsError::IsDir);
                            }
                            if target_dir && !children(&model, &target).is_empty() {
                                return Err(FsError::NotEmpty);
                            }
                            Ok((existing, Some(target)))
                        }
                        None => Ok((key.to_string(), None)),
                    }
                })();
                let got = volume.rename(&from_dir, &from_name, &to_dir, wanted);
                let expected_name = expected.clone().map(|(name, _)| name);
                // Renaming an entry onto its own exact name changes nothing.
                assert_eq!(got, expected_name, "{}: rename {:?} to {:?} in {:?}", context, source, wanted, to_dir);
                if let Ok((new_name, replaced)) = expected {
                    if let Some(replaced) = replaced {
                        model.remove(&replaced);
                    }
                    rekey(&mut model, &source, &join(&to_dir, &new_name));
                }
            }
            8 if !files.is_empty() => {
                let file = pick(&mut random, &files).clone();
                assert_eq!(read_all(&mut volume, &file).as_ref(), Ok(model[&file].as_ref().unwrap()), "{}: read {:?}", context, file);
            }
            9 => {
                let mut listed: Vec<String> = volume.list(&at).unwrap().into_iter().map(|entry| entry.name).collect();
                listed.sort();
                assert_eq!(listed, children(&model, &at), "{}: list {:?}", context, at);
            }
            10 => volume.sync().unwrap(),
            _ => {}
        }
    }
    assert_eq!(differences(&model, &walk_ours(&mut volume)), Vec::<String>::new(), "seed {}: the whole tree", seed);
    volume.sync().unwrap();
    save(volume, &path);
    let (code, output) = macos::fsck(&path);
    assert_eq!(macos::findings(&output), Vec::<String>::new(), "seed {}: fsck_msdos -n said:\n{}", seed, output);
    assert_eq!(code, 0);
    if with_mac {
        let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
        let seen = walk_mac(&mounted.point);
        drop(mounted);
        assert_eq!(differences(&model, &seen), Vec::<String>::new(), "seed {}: what the Mac read", seed);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn random_operations_match_a_model() {
    let seeds: u64 = std::env::var("MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    for seed in 1..=seeds {
        run_seed(seed, 600, seed % 10 == 0);
    }
}
