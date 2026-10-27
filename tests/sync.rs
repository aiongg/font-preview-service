use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use craft_font_store::sha256_hex;
use font_preview_service::store::{Kind, Store};
use font_preview_service::sync::{Config, Fetch, run};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const COMMIT: &str = "45e3d5269e4a17d823f6d84470bc084dc5ae46f6";
const ABEEZEE: &[u8] = include_bytes!("data/ABeeZee-Regular.ttf");
const BARTLE: &[u8] = include_bytes!("data/BBHBartle-Regular.ttf");
const OFL: &[u8] = include_bytes!("data/OFL.txt");

struct Server {
    files: HashMap<String, Vec<u8>>,
    /// Paths jsDelivr refuses with 403, as it does for large repositories.
    refused_on_jsdelivr: HashSet<String>,
    font_hits: AtomicUsize,
}

/// Starts a server on 127.0.0.1 in its own thread and returns its port.
fn serve(server: Arc<Server>) -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/{origin}/{*path}", get(handle))
                .with_state(server);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

async fn handle(
    State(s): State<Arc<Server>>,
    Path((origin, path)): Path<(String, String)>,
) -> Result<Vec<u8>, StatusCode> {
    if path.ends_with(".ttf") || path.ends_with("OFL.txt") {
        s.font_hits.fetch_add(1, Ordering::Relaxed);
    }
    if origin == "jsd" && s.refused_on_jsdelivr.contains(&path) {
        return Err(StatusCode::FORBIDDEN);
    }
    s.files.get(&path).cloned().ok_or(StatusCode::NOT_FOUND)
}

/// Plain HTTP to the local server; the production client is https only. The mirror hosts map to
/// `/jsd/<path in repo>` and `/raw/<path in repo>`.
struct Local(u16);

impl Fetch for Local {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let local = if let Some(rest) = url.strip_prefix("https://cdn.jsdelivr.net/gh/") {
            let after_at = rest.split_once('@').ok_or("bad jsdelivr url")?.1;
            format!(
                "jsd/{}",
                after_at.split_once('/').ok_or("bad jsdelivr url")?.1
            )
        } else if let Some(rest) = url.strip_prefix("https://raw.githubusercontent.com/") {
            let mut parts = rest.splitn(4, '/');
            format!("raw/{}", parts.nth(3).ok_or("bad raw url")?)
        } else {
            return Err(format!("unexpected host in {url}"));
        };
        let mut resp = ureq::get(&format!("http://127.0.0.1:{}/{local}", self.0))
            .call()
            .map_err(|e| e.to_string())?;
        resp.body_mut()
            .with_config()
            .limit(max + 1)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

struct Fixture {
    server: Arc<Server>,
    sources_sha256: String,
}

/// Two families: ABeeZee with a licence file, BBH Bartle with none and refused by jsDelivr.
fn fixture() -> Fixture {
    let file = |path: &str, bytes: &[u8]| json!({"path": path, "size": bytes.len(), "sha256": sha256_hex(bytes), "style": "normal", "weight": 400, "axes": [], "postscript": []});
    let index = json!({
        "schema": 1, "version": "1.0", "source": "google/fonts", "commit": COMMIT,
        "base_url": format!("https://cdn.jsdelivr.net/gh/google/fonts@{COMMIT}/"),
        "fallback_base_url": format!("https://raw.githubusercontent.com/google/fonts/{COMMIT}/"),
        "families": [
            {"family": "ABeeZee", "license": "OFL", "license_path": "ofl/abeezee/OFL.txt",
             "license_file": {"path": "ofl/abeezee/OFL.txt", "size": OFL.len(), "sha256": sha256_hex(OFL)},
             "files": [file("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE)]},
            {"family": "BBH Bartle", "license": "OFL", "license_path": null,
             "files": [file("ofl/bbhbartle/BBHBartle-Regular.ttf", BARTLE)]},
        ]
    })
    .to_string()
    .into_bytes();
    let gz = gzip(&index);
    let sources = json!({"version": "1.0", "indexes": [{
        "source": "google/fonts", "name": "Google Fonts", "curated": true,
        "path": "indexes/google-fonts.json", "size": index.len(), "sha256": sha256_hex(&index),
        "gz_path": "indexes/google-fonts.json.gz", "gz_size": gz.len(), "gz_sha256": sha256_hex(&gz),
        "commit": COMMIT,
    }]})
    .to_string()
    .into_bytes();
    let files = [
        ("sources.json", sources.clone()),
        ("indexes/google-fonts.json.gz", gz),
        ("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec()),
        ("ofl/abeezee/OFL.txt", OFL.to_vec()),
        ("ofl/bbhbartle/BBHBartle-Regular.ttf", BARTLE.to_vec()),
    ];
    let server = Arc::new(Server {
        files: files.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        refused_on_jsdelivr: HashSet::from(["ofl/bbhbartle/BBHBartle-Regular.ttf".to_string()]),
        font_hits: AtomicUsize::new(0),
    });
    Fixture {
        server,
        sources_sha256: sha256_hex(&sources),
    }
}

fn config(dir: &std::path::Path, sha: &str) -> Config {
    Config {
        fonts_dir: dir.to_path_buf(),
        repo: "aiongg/craft-font-index".into(),
        commit: COMMIT.into(),
        sources_sha256: sha.into(),
    }
}

#[test]
fn sync_stores_files_then_skips_them() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);

    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.failures.is_empty(), "{:?}", m.failures);
    assert_eq!((m.totals.fetched, m.totals.skipped), (3, 0));
    assert_eq!(
        (
            m.sources[0].families,
            m.sources[0].files,
            m.sources[0].licences
        ),
        (2, 2, 1)
    );

    let store = Store::new(dir.path());
    assert!(store.has(Kind::File, &sha256_hex(ABEEZEE), ABEEZEE.len() as u64));
    assert!(
        store.has(Kind::File, &sha256_hex(BARTLE), BARTLE.len() as u64),
        "fetched from raw after jsDelivr refused"
    );
    assert!(store.has(Kind::Licence, &sha256_hex(OFL), OFL.len() as u64));
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(store.manifest_path()).unwrap()).unwrap();
    assert_eq!(on_disk["index_commit"], COMMIT);
    assert_eq!(on_disk["sources_sha256"], fx.sources_sha256);

    let hits = fx.server.font_hits.load(Ordering::Relaxed);
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert_eq!(
        (m.totals.fetched, m.totals.skipped, m.totals.failed),
        (0, 3, 0)
    );
    assert_eq!(
        fx.server.font_hits.load(Ordering::Relaxed),
        hits,
        "a second run fetches no font or licence"
    );
}

#[test]
fn sync_refuses_a_sources_file_that_does_not_match_the_pin() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let err = run(&config(dir.path(), &"0".repeat(64)), &Local(port), &|_| {}).unwrap_err();
    assert!(err.contains("INDEX_SHA256"), "{err}");
    assert!(!Store::new(dir.path()).manifest_path().exists());
}

#[test]
fn sync_reports_a_file_no_mirror_has() {
    let mut fx = fixture();
    Arc::get_mut(&mut fx.server)
        .unwrap()
        .files
        .remove("ofl/bbhbartle/BBHBartle-Regular.ttf");
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let m = run(
        &config(dir.path(), &fx.sources_sha256),
        &Local(port),
        &|_| {},
    )
    .unwrap();
    assert_eq!(m.totals.failed, 1);
    assert_eq!(m.failures[0].family, "BBH Bartle");
    assert_eq!(m.totals.fetched, 2);
}

#[test]
fn a_failed_run_keeps_the_last_good_manifest() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);
    run(&cfg, &Local(port), &|_| {}).unwrap();
    let store = Store::new(dir.path());
    let good = std::fs::read(store.manifest_path()).unwrap();

    let mut broken = fixture();
    Arc::get_mut(&mut broken.server)
        .unwrap()
        .files
        .remove("ofl/bbhbartle/BBHBartle-Regular.ttf");
    let port = serve(broken.server.clone());
    // A fresh store, so the missing file is not already present.
    let dir2 = tempfile::tempdir().unwrap();
    let store2 = Store::new(dir2.path());
    std::fs::write(store2.manifest_path(), &good).unwrap();
    let m = run(
        &config(dir2.path(), &broken.sources_sha256),
        &Local(port),
        &|_| {},
    )
    .unwrap();
    assert_eq!(m.totals.failed, 1);
    assert_eq!(std::fs::read(store2.manifest_path()).unwrap(), good);
    let failed: Value =
        serde_json::from_slice(&std::fs::read(store2.failed_manifest_path()).unwrap()).unwrap();
    assert_eq!(failed["totals"]["failed"], 1);
}
