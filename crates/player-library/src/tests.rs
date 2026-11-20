use super::*;
use crate::{metadata, scanner::Scanner};
use player_domain::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "auvra-library-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn root(&self) -> PathBuf {
        let p = self.0.join("music");
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn grant(&self) -> AuthorizedRoot {
        AuthorizedRoot {
            id: "fixture-root".into(),
            name: "Music".into(),
            path: Some(self.root().to_str().unwrap().into()),
        }
    }
    fn db(&self) -> Database {
        let mut db = Database::open(&self.0.join("database")).unwrap();
        db.initialize(&[self.grant()]).unwrap();
        db
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
const FIRST: &[u8] = include_bytes!("../tests/fixtures/tagged-first.flac");
const SECOND: &[u8] = include_bytes!("../tests/fixtures/tagged-second.flac");
fn fixtures(root: &Path) {
    fs::create_dir_all(root.join("Album/Disc 1")).unwrap();
    fs::write(root.join("Album/Disc 1/02.flac"), FIRST).unwrap();
    fs::write(root.join("Album/01.flac"), SECOND).unwrap();
}
fn scan(db: &mut Database, id: &str) -> ScanProgress {
    let root = db
        .roots()
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap();
    let mut scan = Scanner::start(&root, 0, db).unwrap();
    while !scan.step(db).unwrap() {}
    scan.finish(db).unwrap()
}
fn query(mode: LibraryMode) -> LibraryQuery {
    LibraryQuery {
        query_id: 1,
        mode,
        limit: 100,
        ..Default::default()
    }
}
#[test]
fn metadata_artwork_album_order_incremental_cache_and_restart() {
    let temp = Temp::new();
    fixtures(&temp.root());
    let mut db = temp.db();
    let first = scan(&mut db, "fixture-root");
    assert_eq!((first.imported, first.failed), (2, 0));
    let tracks = db.query(&query(LibraryMode::Tracks), 1).unwrap();
    assert_eq!(tracks.total, 2);
    assert_eq!(tracks.tracks[0].item.title, "Тихий берег");
    assert_eq!(tracks.tracks[1].item.title, "First Light");
    assert_eq!(tracks.tracks[0].track_number, Some(1));
    assert_eq!(tracks.tracks[0].disc_number, Some(1));
    assert_eq!(tracks.tracks[0].item.duration_ms, 200);
    assert!(tracks.tracks.iter().all(|t| t.available));
    let cover = PathBuf::from(tracks.tracks[0].item.artwork.as_ref().unwrap());
    assert!(fs::read(&cover).unwrap().starts_with(b"\x89PNG"));
    let albums = db.query(&query(LibraryMode::Albums), 1).unwrap();
    assert_eq!(albums.total, 1);
    assert_eq!(albums.albums[0].artist, "Auvra Sessions");
    assert_eq!(albums.albums[0].year, Some(2026));
    assert_eq!(albums.albums[0].track_count, 2);
    let second = scan(&mut db, "fixture-root");
    assert_eq!((second.imported, second.unchanged), (0, 2));
    fs::remove_file(&cover).unwrap();
    let third = scan(&mut db, "fixture-root");
    assert_eq!(third.imported, 1);
    assert!(cover.is_file());
    drop(db);
    let mut db = Database::open(&temp.0.join("database")).unwrap();
    db.initialize(&[AuthorizedRoot {
        path: None,
        ..temp.grant()
    }])
    .unwrap();
    let unavailable = db.query(&query(LibraryMode::Tracks), 2).unwrap();
    assert_eq!(unavailable.total, 2);
    assert!(
        unavailable
            .tracks
            .iter()
            .all(|t| !t.available && !t.item.capabilities.play)
    );
    assert!(
        db.playback(
            &query(LibraryMode::Tracks),
            unavailable.tracks[0].item.id.as_str()
        )
        .is_err()
    );
    db.initialize(&[temp.grant()]).unwrap();
    assert_eq!(scan(&mut db, "fixture-root").unchanged, 2);
}
#[test]
fn changed_deleted_returned_and_corrupt_files_reconcile_without_losing_metadata() {
    let temp = Temp::new();
    let root = temp.root();
    let path = root.join("track.flac");
    fs::write(&path, FIRST).unwrap();
    let mut db = temp.db();
    scan(&mut db, "fixture-root");
    fs::write(&path, SECOND).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(1)))
        .unwrap();
    assert_eq!(scan(&mut db, "fixture-root").imported, 1);
    let title = db.query(&query(LibraryMode::Tracks), 0).unwrap().tracks[0]
        .item
        .title
        .clone();
    assert_eq!(title, "Тихий берег");
    fs::remove_file(&path).unwrap();
    scan(&mut db, "fixture-root");
    let gone = db.query(&query(LibraryMode::Tracks), 0).unwrap();
    assert_eq!(gone.total, 1);
    assert!(!gone.tracks[0].available);
    fs::write(&path, SECOND).unwrap();
    scan(&mut db, "fixture-root");
    assert!(db.query(&query(LibraryMode::Tracks), 0).unwrap().tracks[0].available);
    fs::write(&path, b"broken FLAC").unwrap();
    assert_eq!(scan(&mut db, "fixture-root").failed, 1);
    let broken = db.query(&query(LibraryMode::Tracks), 0).unwrap();
    assert_eq!(broken.tracks[0].item.title, title);
    assert!(!broken.tracks[0].available);
    assert!(broken.tracks[0].issue.is_some());
    fs::write(&path, FIRST).unwrap();
    assert_eq!(scan(&mut db, "fixture-root").imported, 1);
    assert!(db.query(&query(LibraryMode::Tracks), 0).unwrap().tracks[0].available);
}
#[test]
fn overlapping_roots_deduplicate_and_removal_only_deletes_the_index() {
    let temp = Temp::new();
    let root = temp.root();
    fixtures(&root);
    let mut db = temp.db();
    scan(&mut db, "fixture-root");
    db.add_root(&AuthorizedRoot {
        id: "inner".into(),
        name: "Album".into(),
        path: Some(root.join("Album/Disc 1").to_str().unwrap().into()),
    })
    .unwrap();
    scan(&mut db, "inner");
    assert_eq!(db.query(&query(LibraryMode::Tracks), 0).unwrap().total, 2);
    db.remove_root("fixture-root").unwrap();
    assert_eq!(db.query(&query(LibraryMode::Tracks), 0).unwrap().total, 1);
    db.remove_root("inner").unwrap();
    db.prune_artwork().unwrap();
    assert_eq!(db.query(&query(LibraryMode::Tracks), 0).unwrap().total, 0);
    assert!(root.join("Album/01.flac").is_file());
}
#[test]
fn pagination_unicode_and_literal_search_use_bound_queries() {
    let temp = Temp::new();
    fixtures(&temp.root());
    let mut db = temp.db();
    scan(&mut db, "fixture-root");
    let mut q = query(LibraryMode::Tracks);
    q.text = "ТИХИЙ".into();
    let page = db.query(&q, 7).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.revision, 7);
    q.text = "%' OR 1=1 --".into();
    assert_eq!(db.query(&q, 0).unwrap().total, 0);
    q.text.clear();
    q.limit = 1;
    let first = db.query(&q, 0).unwrap();
    q.offset = 1;
    let second = db.query(&q, 0).unwrap();
    assert_eq!(first.total, 2);
    assert_ne!(first.tracks[0].item.id, second.tracks[0].item.id);
    q.limit = MAX_LIBRARY_PAGE + 1;
    assert!(db.query(&q, 0).is_err());
}
#[cfg(unix)]
#[test]
fn recursive_walk_does_not_follow_symlink_cycles_or_escape_the_granted_root() {
    let temp = Temp::new();
    let root = temp.root();
    fixtures(&root);
    fs::write(temp.0.join("outside.flac"), FIRST).unwrap();
    std::os::unix::fs::symlink(&root, root.join("Album/loop")).unwrap();
    std::os::unix::fs::symlink(temp.0.join("outside.flac"), root.join("escape.flac")).unwrap();
    let mut db = temp.db();
    let result = scan(&mut db, "fixture-root");
    assert_eq!(result.imported, 2);
    assert_eq!(db.query(&query(LibraryMode::Tracks), 0).unwrap().total, 2);
}
#[test]
fn incomplete_inventory_preserves_unvisited_tracks_and_newer_schema_is_rejected() {
    let temp = Temp::new();
    fixtures(&temp.root());
    let mut db = temp.db();
    scan(&mut db, "fixture-root");
    db.finish_scan("fixture-root", 123, false, Some("partial inventory"))
        .unwrap();
    assert!(
        db.query(&query(LibraryMode::Tracks), 0)
            .unwrap()
            .tracks
            .iter()
            .all(|t| t.available)
    );
    drop(db);
    let connection = rusqlite::Connection::open(temp.0.join("database/library.sqlite3")).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    assert!(Database::open(&temp.0.join("database")).is_err());
    assert!(temp.root().join("Album/01.flac").is_file());
    let path = temp.0.join("not-a-directory");
    fs::write(&path, b"keep me").unwrap();
    assert!(Database::open(&path).is_err());
    assert_eq!(fs::read(path).unwrap(), b"keep me");
}
fn wait(worker: &mut LibraryWorker, predicate: impl Fn(&LibraryUpdate) -> bool) -> LibraryUpdate {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(update) = worker.poll() {
            if let LibraryUpdate::Error(e) = &update {
                panic!("{}", e.message);
            }
            if predicate(&update) {
                return update;
            }
        }
        assert!(Instant::now() < deadline, "library event timed out");
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn worker_cancel_query_resume_and_restart_keep_catalog_and_control_independent() {
    let temp = Temp::new();
    let root = temp.root();
    for n in 0..400 {
        fs::write(root.join(format!("{n}.flac")), FIRST).unwrap();
    }
    let mut worker = LibraryWorker::new().unwrap();
    worker
        .submit(LibraryAction::Initialize {
            directory: temp.0.join("database").to_str().unwrap().into(),
            roots: vec![temp.grant()],
        })
        .unwrap();
    wait(
        &mut worker,
        |u| matches!(u,LibraryUpdate::State(s) if s.scan.is_some()),
    );
    worker.submit(LibraryAction::CancelScan).unwrap();
    wait(
        &mut worker,
        |u| matches!(u,LibraryUpdate::State(s) if s.scan.is_none()&&s.roots.iter().any(|r|r.status=="cancelled")),
    );
    worker
        .submit(LibraryAction::Query(query(LibraryMode::Tracks)))
        .unwrap();
    wait(&mut worker, |u| matches!(u, LibraryUpdate::Page(_)));
    worker.submit(LibraryAction::Scan(None)).unwrap();
    wait(
        &mut worker,
        |u| matches!(u,LibraryUpdate::State(s) if s.scan.is_none()&&s.total_tracks==400&&s.last_scan.as_ref().is_some_and(|s|s.imported+s.unchanged==400)),
    );
    worker
        .submit(LibraryAction::Query(query(LibraryMode::Tracks)))
        .unwrap();
    let page = wait(&mut worker, |u| matches!(u, LibraryUpdate::Page(_)));
    if let LibraryUpdate::Page(p) = page {
        assert_eq!(p.tracks.len(), 100);
        assert_eq!(p.total, 400);
    }
    drop(worker);
    let mut worker = LibraryWorker::new().unwrap();
    worker
        .submit(LibraryAction::Initialize {
            directory: temp.0.join("database").to_str().unwrap().into(),
            roots: vec![temp.grant()],
        })
        .unwrap();
    let update = wait(
        &mut worker,
        |u| matches!(u,LibraryUpdate::State(s) if s.scan.is_none()&&s.last_scan.as_ref().is_some_and(|s|s.unchanged==400)),
    );
    if let LibraryUpdate::State(s) = update {
        assert_eq!(s.total_albums, 1);
    }
}
#[test]
fn fingerprint_tracks_nanosecond_changes() {
    let temp = Temp::new();
    let path = temp.root().join("file.wav");
    fs::write(&path, b"abc").unwrap();
    let before = metadata::fingerprint(&fs::metadata(&path).unwrap()).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(
            SystemTime::UNIX_EPOCH + Duration::from_nanos(before.modified_unix_ns + 1),
        ))
        .unwrap();
    let after = metadata::fingerprint(&fs::metadata(path).unwrap()).unwrap();
    assert_eq!(before.size_bytes, after.size_bytes);
    assert_ne!(before, after);
}

#[test]
fn artwork_cache_failure_does_not_disable_valid_audio_and_can_be_retried() {
    let temp = Temp::new();
    fs::write(temp.root().join("track.flac"), FIRST).unwrap();
    let mut db = temp.db();
    fs::write(&db.cache, b"not a directory").unwrap();
    let progress = scan(&mut db, "fixture-root");
    assert_eq!(progress.imported, 1);
    assert_eq!(progress.failed, 1);
    assert!(db.query(&query(LibraryMode::Tracks), 0).unwrap().tracks[0].available);
    assert!(
        db.roots().unwrap()[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("Artwork")
    );
    fs::remove_file(&db.cache).unwrap();
    let progress = scan(&mut db, "fixture-root");
    assert_eq!(progress.imported, 1);
    assert_eq!(progress.failed, 0);
    assert!(
        db.query(&query(LibraryMode::Tracks), 0).unwrap().tracks[0]
            .item
            .artwork
            .as_ref()
            .is_some_and(|p| Path::new(p).is_file())
    );
}

#[test]
fn scanner_imports_all_enabled_codec_containers() {
    let cases: [(&str, &[u8]); 8] = [
        (
            "signal.mp3",
            include_bytes!("../../audio-engine/tests/fixtures/stereo.mp3"),
        ),
        (
            "signal.ogg",
            include_bytes!("../../audio-engine/tests/fixtures/stereo.ogg"),
        ),
        (
            "aac.m4a",
            include_bytes!("../../audio-engine/tests/fixtures/stereo-aac.m4a"),
        ),
        (
            "alac.m4a",
            include_bytes!("../../audio-engine/tests/fixtures/stereo-alac.m4a"),
        ),
        (
            "alac.alac",
            include_bytes!("../../audio-engine/tests/fixtures/stereo-alac.m4a"),
        ),
        (
            "signal.aiff",
            include_bytes!("../../audio-engine/tests/fixtures/stereo.aiff"),
        ),
        (
            "signal.wav",
            include_bytes!("../../audio-engine/tests/fixtures/mono-22050.wav"),
        ),
        ("signal.flac", FIRST),
    ];
    let temp = Temp::new();
    for (name, data) in cases {
        fs::write(temp.root().join(name), data).unwrap();
    }
    let mut db = temp.db();
    let progress = scan(&mut db, "fixture-root");
    assert_eq!(progress.imported, 8);
    assert_eq!(progress.failed, 0);
    let page = db.query(&query(LibraryMode::Tracks), 0).unwrap();
    assert_eq!(page.total, 8);
    assert!(
        page.tracks
            .iter()
            .all(|t| t.available && t.item.duration_ms > 0)
    );
}

#[test]
fn sqlite_write_failure_ends_scan_and_keeps_the_prior_catalog() {
    let temp = Temp::new();
    fixtures(&temp.root());
    let mut db = temp.db();
    scan(&mut db, "fixture-root");
    drop(db);
    for n in 0..400 {
        fs::write(temp.root().join(format!("new-{n}.flac")), FIRST).unwrap();
    }
    let mut worker = LibraryWorker::new().unwrap();
    worker
        .submit(LibraryAction::Initialize {
            directory: temp.0.join("database").to_str().unwrap().into(),
            roots: vec![temp.grant()],
        })
        .unwrap();
    wait(
        &mut worker,
        |u| matches!(u,LibraryUpdate::State(state) if state.scan.is_some()),
    );
    let connection = rusqlite::Connection::open(temp.0.join("database/library.sqlite3")).unwrap();
    connection.busy_timeout(Duration::from_secs(2)).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut failed = false;
    let mut reconciled = false;
    while Instant::now() < deadline && !(failed && reconciled) {
        match worker.poll() {
            Some(LibraryUpdate::Error(_)) => failed = true,
            Some(LibraryUpdate::State(state)) => reconciled = state.scan.is_none(),
            _ => {}
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(failed && reconciled, "DB failure left a scan pending");
    connection.execute_batch("ROLLBACK").unwrap();
    drop(worker);
    let db = Database::open(&temp.0.join("database")).unwrap();
    assert!(
        db.cached(temp.root().join("Album/01.flac").to_str().unwrap())
            .unwrap()
            .is_some()
    );
    assert!(
        db.cached(temp.root().join("Album/Disc 1/02.flac").to_str().unwrap())
            .unwrap()
            .is_some()
    );
}

#[test]
fn mp3_mp4_and_vorbis_tags_and_embedded_artwork_are_preserved() {
    let cases: [(&str, &[u8]); 3] = [
        ("tagged.mp3", include_bytes!("../tests/fixtures/tagged.mp3")),
        ("tagged.m4a", include_bytes!("../tests/fixtures/tagged.m4a")),
        ("tagged.ogg", include_bytes!("../tests/fixtures/tagged.ogg")),
    ];
    let temp = Temp::new();
    for (name, data) in cases {
        let path = temp.root().join(name);
        fs::write(&path, data).unwrap();
        let (track, warning) = metadata::inspect(&path, &temp.0.join("artwork")).unwrap();
        assert!(warning.is_none(), "{name}");
        assert_eq!(track.item.title, "Metadata Check", "{name}");
        assert_eq!(track.item.artists, ["Auvra Sessions"], "{name}");
        assert_eq!(track.album_title, "Metadata Checks", "{name}");
        assert_eq!(track.album_artist, "Auvra Sessions", "{name}");
        assert_eq!(track.track_number, Some(3), "{name}");
        assert_eq!(track.disc_number, Some(2), "{name}");
        assert_eq!(track.year, Some(2026), "{name}");
        assert!(
            track
                .item
                .artwork
                .as_ref()
                .is_some_and(|p| Path::new(p).is_file()),
            "{name}: embedded cover missing"
        );
    }
}
