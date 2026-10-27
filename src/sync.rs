//! `font-preview-service sync`: mirrors every font and licence file the pinned index lists into the
//! content-addressed store, then writes `manifest.json`.

use crate::store::{Kind, Store};
use craft_font_store::{
    Cancel, Client, FontFile, Index, SourceEntry, SourcesFile, sha256_hex, verify_font,
};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const DEFAULT_INDEX_REPO: &str = "aiongg/craft-font-index";
const WORKERS: usize = 4;
const LOG_EVERY: usize = 100;
const MAX_SOURCES_BYTES: u64 = 1 << 20;
const MAX_INDEX_BYTES: u64 = 32 << 20;
const MAX_LICENCE_BYTES: u64 = 1 << 20;

/// Where bytes come from. Production uses [`HttpFetch`]; tests substitute a local server.
pub trait Fetch: Sync {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String>;
}

pub struct HttpFetch(Client);

impl HttpFetch {
    pub fn new() -> Self {
        Self(Client::default())
    }
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        self.0
            .get(url, max, &Cancel::new(), &mut |_| {})
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub fonts_dir: PathBuf,
    pub repo: String,
    pub commit: String,
    pub sources_sha256: String,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_repo(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with('.')
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    s.split_once('/').is_some_and(|(o, n)| part(o) && part(n))
}

impl Config {
    /// Reads `FONTS_DIR`, `INDEX_REPO`, `INDEX_COMMIT` and `INDEX_SHA256` through `get`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let need = |key: &str| {
            get(key)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{key} is not set"))
        };
        let fonts_dir = PathBuf::from(need("FONTS_DIR")?);
        let repo = get("INDEX_REPO")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_INDEX_REPO.to_string());
        let commit = need("INDEX_COMMIT")?;
        let sources_sha256 = need("INDEX_SHA256")?;
        if !is_repo(&repo) {
            return Err(format!("INDEX_REPO `{repo}` is not owner/name"));
        }
        if !is_hex(&commit, 40) {
            return Err("INDEX_COMMIT must be 40 lowercase hex digits".into());
        }
        if !is_hex(&sources_sha256, 64) {
            return Err("INDEX_SHA256 must be 64 lowercase hex digits".into());
        }
        Ok(Self {
            fonts_dir,
            repo,
            commit,
            sources_sha256,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Base URLs of the index repository at the pinned commit, best first.
    fn bases(&self) -> [String; 2] {
        [
            format!("https://cdn.jsdelivr.net/gh/{}@{}/", self.repo, self.commit),
            format!(
                "https://raw.githubusercontent.com/{}/{}/",
                self.repo, self.commit
            ),
        ]
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SourceReport {
    pub source: String,
    pub families: usize,
    pub files: usize,
    pub licences: usize,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub family: String,
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub fetched: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub index_repo: String,
    pub index_commit: String,
    pub sources_sha256: String,
    pub sources: Vec<SourceReport>,
    pub totals: Totals,
    pub failures: Vec<Failure>,
}

/// Tries each URL in turn and returns the first body `accept` takes. A refusal (403 from jsDelivr
/// for large repositories) or a bad body moves on to the next URL.
fn fetch_first<T>(
    fetch: &dyn Fetch,
    urls: &[String],
    max: u64,
    accept: &dyn Fn(&[u8]) -> Result<T, String>,
) -> Result<T, String> {
    let mut last = "no URL to try".to_string();
    for url in urls {
        match fetch.get(url, max).and_then(|bytes| accept(&bytes)) {
            Ok(v) => return Ok(v),
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(last)
}

fn urls_for(bases: &[String], path: &str) -> Vec<String> {
    bases.iter().map(|b| format!("{b}{path}")).collect()
}

fn read_sources(cfg: &Config, fetch: &dyn Fetch) -> Result<SourcesFile, String> {
    let urls = urls_for(&cfg.bases(), "sources.json");
    fetch_first(fetch, &urls, MAX_SOURCES_BYTES, &|bytes| {
        if sha256_hex(bytes) != cfg.sources_sha256 {
            return Err("sources.json does not match INDEX_SHA256".into());
        }
        SourcesFile::from_json(bytes).map_err(|e| e.to_string())
    })
}

fn read_index(entry: &SourceEntry, bases: &[String], fetch: &dyn Fetch) -> Result<Index, String> {
    let (path, gz) = match entry.gz() {
        Some((path, _, _)) => (path, true),
        None => (entry.path.as_str(), false),
    };
    fetch_first(fetch, &urls_for(bases, path), MAX_INDEX_BYTES, &|bytes| {
        entry.read(bytes, gz).map_err(|e| e.to_string())
    })
}

struct Job {
    family: String,
    label: String,
    kind: Kind,
    sha256: String,
    size: u64,
    urls: Vec<String>,
    font: Option<FontFile>,
}

/// One job per distinct blob of the index; `seen` spans all sources so shared blobs are fetched once.
fn collect_jobs(
    source: &str,
    index: &Index,
    seen: &mut HashSet<(Kind, String)>,
) -> (SourceReport, Vec<Job>) {
    let mut report = SourceReport {
        source: source.to_string(),
        ..SourceReport::default()
    };
    let mut jobs = Vec::new();
    for family in index.families() {
        report.families += 1;
        for file in &family.files {
            report.files += 1;
            if seen.insert((Kind::File, file.sha256.clone())) {
                jobs.push(Job {
                    family: family.family.clone(),
                    label: file.path.clone(),
                    kind: Kind::File,
                    sha256: file.sha256.clone(),
                    size: file.size,
                    urls: index.file_urls(family, file),
                    font: Some(file.clone()),
                });
            }
        }
        if let Some(lic) = &family.license_file {
            report.licences += 1;
            if seen.insert((Kind::Licence, lic.sha256.clone())) {
                jobs.push(Job {
                    family: family.family.clone(),
                    label: lic.path.clone(),
                    kind: Kind::Licence,
                    sha256: lic.sha256.clone(),
                    size: lic.size,
                    urls: index.license_urls(family),
                    font: None,
                });
            }
        }
    }
    (report, jobs)
}

enum Outcome {
    Skipped,
    Fetched,
    Failed(String),
}

fn check_body(job: &Job, bytes: &[u8]) -> Result<(), String> {
    if let Some(font) = &job.font {
        return verify_font(font, bytes).map_err(|e| e.to_string());
    }
    if bytes.len() as u64 != job.size {
        return Err(format!("size {} instead of {}", bytes.len(), job.size));
    }
    if sha256_hex(bytes) != job.sha256 {
        return Err("sha256 mismatch".into());
    }
    Ok(())
}

fn process(job: &Job, store: &Store, fetch: &dyn Fetch) -> Outcome {
    if store.has(job.kind, &job.sha256, job.size) {
        return Outcome::Skipped;
    }
    let max = if job.kind == Kind::File {
        job.size
    } else {
        job.size.min(MAX_LICENCE_BYTES)
    };
    let body = fetch_first(fetch, &job.urls, max, &|bytes| {
        check_body(job, bytes).map(|()| bytes.to_vec())
    });
    match body {
        Ok(bytes) => match store.put(job.kind, &job.sha256, &bytes) {
            Ok(()) => Outcome::Fetched,
            Err(e) => Outcome::Failed(format!("writing failed: {e}")),
        },
        Err(reason) => Outcome::Failed(reason),
    }
}

fn run_jobs(
    jobs: &[Job],
    store: &Store,
    fetch: &dyn Fetch,
    log: &(dyn Fn(&str) + Sync),
) -> (Totals, Vec<Failure>) {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results = Mutex::new((Totals::default(), Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = process(job, store, fetch);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Outcome::Failed(reason) = &outcome {
                        log(&format!("FAILED {} / {}: {reason}", job.family, job.label));
                    }
                    if n.is_multiple_of(LOG_EVERY) {
                        log(&format!("{n}/{} files handled", jobs.len()));
                    }
                    let Ok(mut guard) = results.lock() else {
                        return;
                    };
                    let (totals, failures) = &mut *guard;
                    match outcome {
                        Outcome::Skipped => totals.skipped += 1,
                        Outcome::Fetched => totals.fetched += 1,
                        Outcome::Failed(reason) => {
                            totals.failed += 1;
                            failures.push(Failure {
                                family: job.family.clone(),
                                file: job.label.clone(),
                                reason,
                            });
                        }
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs the sync. `Err` means the index could not be read at all; per-file problems are in the
/// returned manifest's `failures`.
pub fn run(
    cfg: &Config,
    fetch: &dyn Fetch,
    log: &(dyn Fn(&str) + Sync),
) -> Result<Manifest, String> {
    let store = Store::new(&cfg.fonts_dir);
    let sources = read_sources(cfg, fetch).map_err(|e| format!("reading sources.json: {e}"))?;
    log(&format!(
        "sources.json lists {} indexes",
        sources.indexes.len()
    ));
    let bases = cfg.bases();
    let mut seen = HashSet::new();
    let (mut reports, mut jobs, mut failures) = (Vec::new(), Vec::new(), Vec::new());
    for entry in &sources.indexes {
        match read_index(entry, &bases, fetch) {
            Ok(index) => {
                let (report, found) = collect_jobs(&entry.source, &index, &mut seen);
                log(&format!(
                    "{}: {} families, {} files",
                    entry.source, report.families, report.files
                ));
                reports.push(report);
                jobs.extend(found);
            }
            Err(reason) => {
                log(&format!("FAILED index {}: {reason}", entry.source));
                failures.push(Failure {
                    family: entry.source.clone(),
                    file: entry.path.clone(),
                    reason,
                });
            }
        }
    }
    let (totals, file_failures) = run_jobs(&jobs, &store, fetch, log);
    failures.extend(file_failures);
    let totals = Totals {
        failed: failures.len(),
        ..totals
    };
    let manifest = Manifest {
        index_repo: cfg.repo.clone(),
        index_commit: cfg.commit.clone(),
        sources_sha256: cfg.sources_sha256.clone(),
        sources: reports,
        totals,
        failures,
    };
    write_manifest(&store, &manifest)?;
    log(&format!(
        "done: {} fetched, {} already present, {} failed",
        manifest.totals.fetched, manifest.totals.skipped, manifest.totals.failed
    ));
    Ok(manifest)
}

fn write_manifest(store: &Store, manifest: &Manifest) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(store.root())
        .map_err(|e| format!("creating {}: {e}", store.root().display()))?;
    let path = store.manifest_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The `sync` subcommand. Returns the process exit code.
pub fn main() -> i32 {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("sync: {e}");
            return 2;
        }
    };
    match run(&cfg, &HttpFetch::new(), &|line| println!("{line}")) {
        Ok(m) if m.failures.is_empty() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("sync: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn good() -> Vec<(&'static str, String)> {
        vec![
            ("FONTS_DIR", "/data".into()),
            ("INDEX_COMMIT", COMMIT.into()),
            ("INDEX_SHA256", "a".repeat(64)),
        ]
    }

    fn lookup(pairs: &[(&str, String)]) -> Result<Config, String> {
        let owned: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Config::from_lookup(env(&owned))
    }

    #[test]
    fn config_defaults_the_repo() {
        let cfg = lookup(&good()).unwrap();
        assert_eq!(cfg.repo, DEFAULT_INDEX_REPO);
        assert_eq!(
            cfg.bases()[0],
            format!("https://cdn.jsdelivr.net/gh/aiongg/craft-font-index@{COMMIT}/")
        );
        assert_eq!(
            cfg.bases()[1],
            format!("https://raw.githubusercontent.com/aiongg/craft-font-index/{COMMIT}/")
        );
    }

    #[test]
    fn config_refuses_missing_or_malformed_pins() {
        let without = |key: &str| {
            good()
                .into_iter()
                .filter(|(k, _)| *k != key)
                .collect::<Vec<_>>()
        };
        assert!(lookup(&without("INDEX_COMMIT")).is_err());
        assert!(lookup(&without("INDEX_SHA256")).is_err());
        assert!(lookup(&without("FONTS_DIR")).is_err());
        for (key, bad) in [
            ("INDEX_COMMIT", "main"),
            ("INDEX_COMMIT", "ABCDEF0123456789abcdef0123456789abcdef01"),
            ("INDEX_SHA256", "abc"),
            ("INDEX_REPO", "no-slash"),
            ("INDEX_REPO", "a/../b"),
        ] {
            let mut pairs = without(key);
            pairs.push((key, bad.to_string()));
            assert!(lookup(&pairs).is_err(), "{key}={bad}");
        }
    }

    struct Failing;
    impl Fetch for Failing {
        fn get(&self, url: &str, _: u64) -> Result<Vec<u8>, String> {
            Err(format!("no route to {url}"))
        }
    }

    #[test]
    fn present_blobs_are_skipped_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let bytes = b"licence text";
        let sha = sha256_hex(bytes);
        store.put(Kind::Licence, &sha, bytes).unwrap();
        let job = Job {
            family: "f".into(),
            label: "OFL.txt".into(),
            kind: Kind::Licence,
            sha256: sha,
            size: bytes.len() as u64,
            urls: vec!["https://example.invalid/x".into()],
            font: None,
        };
        assert!(matches!(process(&job, &store, &Failing), Outcome::Skipped));
        let missing = Job {
            sha256: sha256_hex(b"other"),
            size: 5,
            ..job
        };
        assert!(matches!(
            process(&missing, &store, &Failing),
            Outcome::Failed(_)
        ));
    }
}
