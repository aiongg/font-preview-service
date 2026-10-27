//! Static preview subsets for every face, written into a generation directory at the end of a
//! clean sync, with the checks of architecture §4 ("Checks at every sync").

use crate::index12;
use crate::store::{Kind, Store};
use crate::subset::{self, Instance, Licence, Naming, Quality, closure, codepoints};
use serde::Serialize;
use skrifa::{FontRef, MetadataProvider, Tag};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The `{inst}` names of the static files.
pub const SAMPLE: &str = "sample";
pub const ASCII: &str = "ascii";
pub const VAR: &str = "var";
/// Report entries for a face that fails before any subset is made.
pub const ALL: &str = "*";

/// One distinct font file to precompute, with what its index says about it.
#[derive(Debug, Clone)]
pub struct Face {
    pub sha256: String,
    pub family: String,
    pub license: String,
    pub reserved_names: Vec<String>,
    pub sample_text: Option<String>,
    /// The instance the font list shows, from the index's `list_instance`.
    pub list: Instance,
    pub variable: bool,
}

/// Adds the faces of `index` not already in `seen` (by sha256), so a file several indexes list is
/// precomputed once, with the first index's family.
pub fn collect_faces(index: &index12::Index, seen: &mut HashSet<String>, faces: &mut Vec<Face>) {
    for family in &index.families {
        for file in &family.files {
            if !seen.insert(file.sha256.clone()) {
                continue;
            }
            faces.push(Face {
                sha256: file.sha256.clone(),
                family: family.family.clone(),
                license: family.license.clone(),
                reserved_names: family.reserved_names.clone(),
                sample_text: index.sample_text(family, file).map(str::to_owned),
                list: list_instance(file),
                variable: !file.axes.is_empty(),
            });
        }
    }
}

/// The index's `list_instance` as axis values. Without one, a file whose index lists instances
/// shows the default location; a file without instance data falls back to the subset module's
/// own rule, which is the same rule.
fn list_instance(file: &index12::File) -> Instance {
    match file.list_instance.and_then(|i| file.instances.get(i)) {
        Some(inst) => Instance::Named(
            inst.coordinates
                .iter()
                .filter_map(|(tag, v)| Some((Tag::new_checked(tag.as_bytes()).ok()?, *v)))
                .collect(),
        ),
        None if !file.instances.is_empty() => Instance::Named(Vec::new()),
        None => Instance::List,
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Failure {
    pub sha256: String,
    pub family: String,
    /// `sample`, `ascii`, `var`, or `*` when the face failed before any subset.
    pub inst: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub faces: usize,
    pub written: Written,
    pub failures: Vec<Failure>,
}

#[derive(Debug, Default, Serialize)]
pub struct Written {
    pub sample: usize,
    pub ascii: usize,
    pub var: usize,
}

/// Where a static subset lives inside a generation directory.
pub fn static_path(generation: &Path, sha256: &str, inst: &str) -> std::path::PathBuf {
    generation
        .join("static")
        .join(sha256)
        .join(format!("{inst}.woff2"))
}

/// Precomputes every face into `generation`, on up to `available_parallelism` threads.
pub fn run(faces: &[Face], store: &Store, generation: &Path) -> Report {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(faces.len())
        .max(1);
    let next = AtomicUsize::new(0);
    let report = Mutex::new(Report {
        faces: faces.len(),
        ..Report::default()
    });
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(face) = faces.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = precompute_face(face, store, generation);
                    let Ok(mut report) = report.lock() else {
                        return;
                    };
                    for (inst, result) in outcome {
                        match result {
                            Ok(()) => match inst {
                                SAMPLE => report.written.sample += 1,
                                ASCII => report.written.ascii += 1,
                                _ => report.written.var += 1,
                            },
                            Err(reason) => report.failures.push(Failure {
                                sha256: face.sha256.clone(),
                                family: face.family.clone(),
                                inst: inst.to_owned(),
                                reason,
                            }),
                        }
                    }
                }
            });
        }
    });
    let mut report = report
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    report
        .failures
        .sort_by(|a, b| (&a.family, &a.sha256, &a.inst).cmp(&(&b.family, &b.sha256, &b.inst)));
    report
}

fn licence(face: &Face) -> Result<Licence, String> {
    match face.license.as_str() {
        "OFL" => Ok(Licence::Ofl {
            reserved_names: face.reserved_names.clone(),
        }),
        "UFL" => Ok(Licence::Ufl),
        "APACHE2" => Ok(Licence::Apache),
        other => Err(format!("unknown licence `{other}`")),
    }
}

fn sha256_bytes(hex: &str) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    if hex.len() != 64 {
        return Err(format!("sha256 `{hex}` is not 64 hex digits"));
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("sha256 `{hex}` is not hex"))?;
    }
    Ok(out)
}

type Outcome = Vec<(&'static str, Result<(), String>)>;

fn precompute_face(face: &Face, store: &Store, generation: &Path) -> Outcome {
    let setup = || -> Result<(Vec<u8>, Naming), String> {
        let naming = Naming {
            licence: licence(face)?,
            family_name: face.family.clone(),
            sha256: sha256_bytes(&face.sha256)?,
        };
        let path = store.path(Kind::File, &face.sha256);
        let font = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok((font, naming))
    };
    let (font, naming) = match setup() {
        Ok(v) => v,
        Err(reason) => return vec![(ALL, Err(reason))],
    };
    let full = match FontRef::new(&font) {
        Ok(full) => full,
        Err(e) => return vec![(ALL, Err(format!("cannot read font: {e}")))],
    };
    let sample_texts = face
        .sample_text
        .as_ref()
        .map(|text| vec![text.clone(), face.family.clone()]);
    let make = |inst: &'static str, texts: Option<&[String]>, instance: &Instance| {
        let result = texts
            .ok_or_else(|| "the index gives no sample text".to_string())
            .and_then(|texts| make_one(&font, &full, texts, instance, &naming))
            .and_then(|woff2| write_static(&static_path(generation, &face.sha256, inst), &woff2));
        (inst, result)
    };
    let mut outcome = vec![make(SAMPLE, sample_texts.as_deref(), &face.list)];
    if covers_latin(&full) {
        outcome.push(make(ASCII, Some(&[ascii()]), &face.list));
    }
    if face.variable {
        outcome.push(make(VAR, sample_texts.as_deref(), &Instance::Var));
    }
    outcome
}

fn ascii() -> String {
    (0x20u8..=0x7e).map(char::from).collect()
}

/// Whether the cmap maps at least 90% of A–Z and a–z.
fn covers_latin(font: &FontRef) -> bool {
    let charmap = font.charmap();
    let mapped = ('A'..='Z')
        .chain('a'..='z')
        .filter(|&c| charmap.map(c).is_some())
        .count();
    mapped * 10 >= 52 * 9
}

/// Subsets `texts` (with the closure), checks the subset against the full font and returns it as
/// WOFF2.
fn make_one(
    font: &[u8],
    full: &FontRef,
    texts: &[String],
    instance: &Instance,
    naming: &Naming,
) -> Result<Vec<u8>, String> {
    let cps: Vec<u32> = texts
        .iter()
        .flat_map(|t| codepoints::of_text(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sfnt = subset::subset_renamed(font, &closure(&cps), instance, naming)
        .map_err(|e| e.to_string())?;
    check(font, full, &sfnt, &cps, texts)?;
    subset::encode_woff2(&sfnt, Quality::Static).map_err(|e| e.to_string())
}

/// (a) the subset maps every character of `cps` the full font maps; (b) shaping `texts` with the
/// subset gives no more `.notdef` glyphs than with the full font.
fn check(
    font: &[u8],
    full: &FontRef,
    sfnt: &[u8],
    cps: &[u32],
    texts: &[String],
) -> Result<(), String> {
    let sub = FontRef::new(sfnt).map_err(|e| format!("cannot read the subset: {e}"))?;
    let (full_map, sub_map) = (full.charmap(), sub.charmap());
    if let Some(cp) = cps
        .iter()
        .find(|&&cp| full_map.map(cp).is_some() && sub_map.map(cp).is_none())
    {
        return Err(format!("the subset does not map U+{cp:04X}"));
    }
    let (full_notdef, sub_notdef) = (notdefs(font, texts)?, notdefs(sfnt, texts)?);
    if sub_notdef > full_notdef {
        return Err(format!(
            "shaping gives {sub_notdef} .notdef glyphs with the subset, {full_notdef} with the font"
        ));
    }
    Ok(())
}

fn notdefs(font: &[u8], texts: &[String]) -> Result<usize, String> {
    let face = rustybuzz::Face::from_slice(font, 0).ok_or("rustybuzz cannot read the font")?;
    Ok(texts
        .iter()
        .map(|text| {
            let mut buffer = rustybuzz::UnicodeBuffer::new();
            buffer.push_str(text);
            buffer.guess_segment_properties();
            rustybuzz::shape(&face, &[], buffer)
                .glyph_infos()
                .iter()
                .filter(|g| g.glyph_id == 0)
                .count()
        })
        .sum())
}

/// Temp name + rename, so a reader never sees a partial file.
fn write_static(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    let tmp = path.with_extension("woff2.tmp");
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("writing {}: {e}", path.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(json: &str) -> index12::File {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn shaping_check_catches_a_subset_without_the_closure() {
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/sarabun/Sarabun-Regular.ttf"
        ))
        .unwrap();
        let full = FontRef::new(&font).unwrap();
        let texts = ["นำ".to_string()];
        let cps: Vec<u32> = texts[0].chars().map(u32::from).collect();
        let naming = Naming {
            licence: Licence::Ofl {
                reserved_names: Vec::new(),
            },
            family_name: "Sarabun".into(),
            sha256: [7; 32],
        };
        let bare: BTreeSet<u32> = cps.iter().copied().collect();
        let without = subset::subset_renamed(&font, &bare, &Instance::List, &naming).unwrap();
        let err = check(&font, &full, &without, &cps, &texts).unwrap_err();
        assert!(err.contains(".notdef"), "{err}");
        let with = subset::subset_renamed(&font, &closure(&cps), &Instance::List, &naming).unwrap();
        check(&font, &full, &with, &cps, &texts).unwrap();
    }

    #[test]
    fn list_instance_follows_the_index() {
        let tag = |s: &[u8; 4]| Tag::new(s);
        assert_eq!(
            list_instance(&file(
                r#"{"instances":[{"name":"Thin","coordinates":{"wght":100}},
                    {"name":"Regular","coordinates":{"wght":400,"wdth":100}}],"list_instance":1}"#
            )),
            Instance::Named(vec![(tag(b"wdth"), 100.0), (tag(b"wght"), 400.0)])
        );
        assert_eq!(
            list_instance(&file(
                r#"{"instances":[{"name":"Thin","coordinates":{"wght":100}}]}"#
            )),
            Instance::Named(Vec::new()),
            "no list_instance: the default location"
        );
        assert_eq!(list_instance(&file("{}")), Instance::List);
    }
}
