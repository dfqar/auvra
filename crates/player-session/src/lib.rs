//! The transport database has one serial owner, independent of scan/decode/control.
use player_domain::*;
use player_queue::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle, Thread},
    time::Duration,
};

const MAX_SESSION_BYTES: usize = 24 * 1024 * 1024;
const MAX_REMOTE_BYTES: usize = 32 * 1024;
enum PendingSave {
    Full(u64, Box<TransportSession>),
    Remote(u64, Option<Box<RemoteSelection>>),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteRecord {
    version: u32,
    state: RemoteRecordState,
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "selection",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum RemoteRecordState {
    Empty,
    Selected(Box<RemoteSelection>),
}
#[derive(Serialize)]
struct LocalRecord<'a> {
    version: u32,
    queue: &'a QueueEngine,
    position_ms: u64,
    volume: f32,
    stopped: bool,
    root_paths: &'a [SessionRoot],
}
#[derive(Default)]
struct Mailbox {
    initialize: Option<(String, SessionAccess)>,
    save: Option<PendingSave>,
    sound: Option<(u64, SoundSettings)>,
    reload_sound: bool,
    validate: Option<(u64, Box<QueueEngine>, SessionAccess)>,
    updates: VecDeque<SessionUpdate>,
    stopping: bool,
}
pub struct SessionWorker {
    mailbox: Arc<Mutex<Mailbox>>,
    wake: Thread,
    worker: Option<JoinHandle<()>>,
}
fn storage(error: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(
        ErrorCode::DatabaseError,
        format!("Queue could not be saved or restored: {error}"),
    )
}
fn check_access(access: &SessionAccess) -> Result<(), PlayerError> {
    let ids: std::collections::HashSet<_> = access.roots.iter().map(|r| &r.id).collect();
    if ids.len() != access.roots.len() {
        return Err(PlayerError::invalid("Duplicate queue root identity"));
    }
    if access.roots.len() > 64 || access.files.len() > 64 {
        return Err(PlayerError::invalid("Too many queue access grants"));
    }
    if access
        .roots
        .iter()
        .any(|r| r.id.is_empty() || r.id.len() > 64)
    {
        return Err(PlayerError::invalid("Invalid queue root identity"));
    }
    for path in access.roots.iter().map(|root| &root.path).chain(
        access
            .files
            .iter()
            .flat_map(|f| std::iter::once(&f.original_path).chain(f.path.iter())),
    ) {
        if !Path::new(path).is_absolute() || path.len() > 3500 || path.contains('\0') {
            return Err(PlayerError::invalid("Invalid queue access path"));
        }
    }
    Ok(())
}
impl SessionWorker {
    pub fn new() -> std::io::Result<Self> {
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let shared = mailbox.clone();
        let worker = thread::Builder::new()
            .name("auvra-session".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&shared)));
                if result.is_err() {
                    publish(
                        &shared,
                        SessionUpdate::Error(storage("Session worker stopped unexpectedly")),
                    );
                }
            })?;
        Ok(Self {
            mailbox,
            wake: worker.thread().clone(),
            worker: Some(worker),
        })
    }
}
impl SessionStore for SessionWorker {
    fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError> {
        if self.worker.as_ref().is_none_or(|w| w.is_finished()) {
            return Err(storage("Session worker is unavailable"));
        }
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        match action {
            SessionAction::SaveSound { serial, settings } => {
                settings.validate()?;
                mailbox.sound = Some((serial, settings));
            }
            SessionAction::ReloadSound => mailbox.reload_sound = true,
            SessionAction::Initialize { directory, access } => {
                check_access(&access)?;
                if !Path::new(&directory).is_absolute()
                    || directory.len() > 3500
                    || directory.contains('\0')
                {
                    return Err(PlayerError::invalid("Invalid session directory"));
                }
                mailbox.initialize = Some((directory, access));
            }
            SessionAction::Save { serial, session } => {
                mailbox.save = Some(PendingSave::Full(serial, session));
            }
            SessionAction::SaveRemote { serial, selection } => {
                if let Some(selected) = &selection {
                    selected.validate()?;
                }
                if let Some(PendingSave::Full(previous, session)) = &mut mailbox.save {
                    *previous = serial;
                    session.version = 2;
                    session.remote = selection.map(|s| *s);
                } else {
                    mailbox.save = Some(PendingSave::Remote(serial, selection));
                }
            }
            SessionAction::Validate {
                revision,
                queue,
                access,
            } => {
                check_access(&access)?;
                mailbox.validate = Some((revision, queue, access));
            }
        }
        drop(mailbox);
        self.wake.unpark();
        Ok(())
    }
    fn poll(&mut self) -> Option<SessionUpdate> {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .updates
            .pop_front()
    }
}
impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopping = true;
        self.wake.unpark();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn publish(mailbox: &Mutex<Mailbox>, update: SessionUpdate) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    mailbox
        .updates
        .retain(|old| std::mem::discriminant(old) != std::mem::discriminant(&update));
    mailbox.updates.push_back(update);
}
fn open(directory: &str) -> Result<Connection, PlayerError> {
    std::fs::create_dir_all(directory).map_err(storage)?;
    let db = Connection::open(Path::new(directory).join("transport.sqlite3")).map_err(storage)?;
    db.busy_timeout(Duration::from_millis(500))
        .map_err(storage)?;
    let version: u32 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if version > 3 {
        return Err(storage(
            "Transport database is from a newer version of Auvra",
        ));
    }
    if version == 0 {
        db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE transport (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL); PRAGMA user_version=1; COMMIT;").map_err(storage)?;
    }
    if version < 2 {
        db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE sound_preferences (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL); PRAGMA user_version=2; COMMIT;").map_err(storage)?;
    }
    if version < 3 {
        db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE remote_selection (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL); PRAGMA user_version=3; COMMIT;").map_err(storage)?;
    }
    db.pragma_update(None, "journal_mode", "WAL")
        .map_err(storage)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    Ok(db)
}
fn load(db: &Connection) -> Result<Option<TransportSession>, PlayerError> {
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM transport WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    let remote = load_remote(db)?;
    let Some(size) = size else {
        return Ok(remote.flatten().map(|selection| TransportSession {
            version: 2,
            remote: Some(selection),
            queue: QueueEngine::default(),
            position_ms: 0,
            volume: 0.7,
            stopped: true,
            root_paths: vec![],
        }));
    };
    if size < 0 || size as usize > MAX_SESSION_BYTES {
        return Err(storage("Saved queue is too large"));
    }
    let payload: String = db
        .query_row("SELECT payload FROM transport WHERE id=1", [], |r| r.get(0))
        .map_err(storage)?;
    let mut session: TransportSession = serde_json::from_str(&payload).map_err(storage)?;
    session.validate().map_err(storage)?;
    if let Some(remote) = remote {
        session.remote = remote;
        if session.remote.is_some() {
            session.version = 2;
        }
    }
    session.validate().map_err(storage)?;
    check_projection(&session.queue)?;
    Ok(Some(session))
}
fn remote_payload(selection: Option<&RemoteSelection>) -> Result<String, PlayerError> {
    if let Some(selected) = selection {
        selected.validate().map_err(storage)?;
        if selected.current.artwork.is_some() {
            return Err(storage("Temporary remote artwork must not be persisted"));
        }
    }
    let state = selection.map_or(RemoteRecordState::Empty, |s| {
        RemoteRecordState::Selected(Box::new(s.clone()))
    });
    let payload = serde_json::to_string(&RemoteRecord { version: 1, state }).map_err(storage)?;
    if payload.len() > MAX_REMOTE_BYTES {
        return Err(storage("Remote selection exceeds 32 KiB"));
    }
    Ok(payload)
}
fn load_remote(db: &Connection) -> Result<Option<Option<RemoteSelection>>, PlayerError> {
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM remote_selection WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    let Some(size) = size else {
        return Ok(None);
    };
    if size < 0 || size as usize > MAX_REMOTE_BYTES {
        return Err(storage("Saved remote selection exceeds 32 KiB"));
    }
    let payload: String = db
        .query_row("SELECT payload FROM remote_selection WHERE id=1", [], |r| {
            r.get(0)
        })
        .map_err(storage)?;
    let record: RemoteRecord = serde_json::from_str(&payload).map_err(storage)?;
    if record.version != 1 {
        return Err(storage(
            "Saved remote selection is from an unsupported version",
        ));
    }
    let selection = match record.state {
        RemoteRecordState::Empty => None,
        RemoteRecordState::Selected(selection) => Some(*selection),
    };
    remote_payload(selection.as_ref())?;
    Ok(Some(selection))
}
fn save_remote(db: &Connection, selection: Option<&RemoteSelection>) -> Result<(), PlayerError> {
    let payload = remote_payload(selection)?;
    db.execute("INSERT INTO remote_selection(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![payload]).map_err(storage)?;
    Ok(())
}
fn check_projection(queue: &QueueEngine) -> Result<(), PlayerError> {
    queue.validate().map_err(storage)?;
    if serde_json::to_vec(&queue.snapshot())
        .map_err(storage)?
        .len()
        > 12 * 1024 * 1024
    {
        return Err(storage("Saved queue projection exceeds the UI budget"));
    }
    Ok(())
}
fn save(db: &Connection, session: &TransportSession) -> Result<(), PlayerError> {
    session.validate().map_err(storage)?;
    check_projection(&session.queue)?;
    let local = LocalRecord {
        version: session.version,
        queue: &session.queue,
        position_ms: session.position_ms,
        volume: session.volume,
        stopped: session.stopped,
        root_paths: &session.root_paths,
    };
    let payload = serde_json::to_string(&local).map_err(storage)?;
    if payload.len() > MAX_SESSION_BYTES {
        return Err(storage("Queue exceeds the persistence budget"));
    }
    let transaction = db.unchecked_transaction().map_err(storage)?;
    transaction.execute("INSERT INTO transport(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![payload]).map_err(storage)?;
    save_remote(&transaction, session.remote.as_ref())?;
    transaction.commit().map_err(storage)?;
    Ok(())
}
fn load_sound(db: &Connection) -> Result<SoundSettings, PlayerError> {
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM sound_preferences WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage)?;
    let Some(size) = size else {
        return Ok(SoundSettings::default());
    };
    if !(0..=32768).contains(&size) {
        return Err(storage("Sound preferences exceed 32 KiB"));
    }
    let payload: String = db
        .query_row(
            "SELECT payload FROM sound_preferences WHERE id=1",
            [],
            |r| r.get(0),
        )
        .map_err(storage)?;
    let value: SoundSettings = serde_json::from_str(&payload).map_err(storage)?;
    value.validate().map_err(storage)?;
    Ok(value)
}
fn save_sound(db: &Connection, settings: &SoundSettings) -> Result<(), PlayerError> {
    settings.validate().map_err(storage)?;
    let payload = serde_json::to_string(settings).map_err(storage)?;
    if payload.len() > 32768 {
        return Err(storage("Sound preferences exceed 32 KiB"));
    }
    db.execute("INSERT INTO sound_preferences(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![payload]).map_err(storage)?;
    Ok(())
}
fn resolve(
    queue: &mut QueueEngine,
    access: &SessionAccess,
    saved_roots: &[SessionRoot],
) -> Vec<QueueIssue> {
    let roots: Vec<PathBuf> = access
        .roots
        .iter()
        .filter_map(|p| std::fs::canonicalize(&p.path).ok())
        .filter(|p| p.is_dir())
        .collect();
    let files: Vec<(&FileGrant, Option<PathBuf>)> = access
        .files
        .iter()
        .map(|f| {
            (
                f,
                f.path.as_ref().and_then(|p| std::fs::canonicalize(p).ok()),
            )
        })
        .collect();
    let mut issues = BTreeMap::new();
    let mut resolved: BTreeMap<String, Result<String, PlayerError>> = BTreeMap::new();
    queue.visit_items_mut(|item| {
        let Some(MediaLocation::File { path }) = &mut item.location else { return; };
        let result = resolved.entry(path.clone()).or_insert_with(|| {
            let mapped = files.iter().find(|(f, canonical)| f.original_path == *path || canonical.as_ref().is_some_and(|p| p.to_str()==Some(path.as_str())));
            let rebased = saved_roots.iter().filter_map(|old| {
                let relative = Path::new(path).strip_prefix(&old.path).ok()?;
                let current = access.roots.iter().find(|root| root.id == old.id)?;
                Some((old.path.len(), Path::new(&current.path).join(relative)))
            }).max_by_key(|(length,_)| *length).map(|(_, path)| path);
            let candidate = mapped.and_then(|(f, _)| f.path.as_deref()).map(PathBuf::from).or(rebased).unwrap_or_else(|| PathBuf::from(path.as_str()));
            let file = std::fs::canonicalize(candidate).map_err(|error| PlayerError::new(if error.kind()==std::io::ErrorKind::NotFound { ErrorCode::FileMissing } else { ErrorCode::PermissionLost }, "A restored queue file is missing or inaccessible. Reopen the file or reconnect its folder."))?;
            if !roots.iter().any(|root| file.starts_with(root)) && !files.iter().any(|(_, allowed)| allowed.as_ref()==Some(&file)) {
                return Err(PlayerError::new(ErrorCode::PermissionLost, "Reconnect the folder or reopen this file to restore queue access."));
            }
            if !file.is_file() { return Err(PlayerError::new(ErrorCode::FileMissing, "Restored queue item is not a regular file")); }
            file.to_str().map(str::to_owned).ok_or_else(|| PlayerError::invalid("Queue file path is not UTF-8"))
        });
        match result { Ok(value) => *path = value.clone(), Err(error) => { issues.insert(item.id.as_str().to_owned(), QueueIssue { id: item.id.clone(), error: error.clone() }); } }
    });
    issues.into_values().collect()
}
fn run(mailbox: &Mutex<Mailbox>) {
    let mut database: Option<Connection> = None;
    let mut directory: Option<String> = None;
    let mut writable = false;
    let mut sound_writable = false;
    let mut current_access = SessionAccess::default();
    let mut known_roots: Vec<SessionRoot> = vec![];
    loop {
        let (initialize, validate, pending_save, pending_sound, reload_sound, stopping) = {
            let mut box_ = mailbox.lock().unwrap_or_else(|e| e.into_inner());
            (
                box_.initialize.take(),
                box_.validate.take(),
                box_.save.take(),
                box_.sound.take(),
                std::mem::take(&mut box_.reload_sound),
                box_.stopping,
            )
        };
        let initializing = initialize.is_some();
        if let Some((path, access)) = initialize {
            writable = false;
            current_access = access.clone();
            let result = (|| {
                if directory.as_deref() != Some(&path) || database.is_none() {
                    database = Some(open(&path)?);
                    directory = Some(path);
                }
                let mut session = load(database.as_ref().unwrap())?;
                known_roots = session
                    .as_ref()
                    .map_or_else(Vec::new, |s| s.root_paths.clone());
                let issues = session
                    .as_mut()
                    .map_or_else(Vec::new, |s| resolve(&mut s.queue, &access, &s.root_paths));
                if let Some(session) = &session {
                    check_projection(&session.queue)?;
                }
                writable = true;
                Ok::<_, PlayerError>(SessionUpdate::Loaded {
                    session: session.map(Box::new),
                    issues,
                })
            })();
            publish(mailbox, result.unwrap_or_else(SessionUpdate::Error));
        }
        if initializing || reload_sound {
            let result = database
                .as_ref()
                .ok_or_else(|| storage("Sound database is unavailable"))
                .and_then(load_sound);
            sound_writable = result.is_ok();
            publish(mailbox, SessionUpdate::SoundLoaded(result));
        }
        if let Some((serial, settings)) = pending_sound {
            let result = database
                .as_ref()
                .filter(|_| sound_writable)
                .ok_or_else(|| {
                    storage("Sound preferences could not be loaded; existing data was preserved")
                })
                .and_then(|db| save_sound(db, &settings));
            publish(mailbox, SessionUpdate::SoundSaved { serial, result });
        }
        if let Some((revision, mut queue, access)) = validate {
            current_access = access.clone();
            let issues = resolve(&mut queue, &access, &known_roots);
            let update = match check_projection(&queue) {
                Ok(()) => SessionUpdate::Validated {
                    revision,
                    queue,
                    issues,
                },
                Err(error) => SessionUpdate::Error(error),
            };
            publish(mailbox, update);
        }
        if let Some(pending) = pending_save {
            let (serial, result) = match pending {
                PendingSave::Remote(serial, selection) => (
                    serial,
                    database
                        .as_ref()
                        .filter(|_| writable)
                        .ok_or_else(|| storage("Session database is not open"))
                        .and_then(|db| save_remote(db, selection.as_deref())),
                ),
                PendingSave::Full(serial, mut session) => {
                    let mut paths = vec![];
                    session.queue.visit_items_mut(|item| {
                        if let Some(MediaLocation::File { path }) = &item.location {
                            paths.push(PathBuf::from(path));
                        }
                    });
                    for root in &current_access.roots {
                        let root = SessionRoot {
                            id: root.id.clone(),
                            path: std::fs::canonicalize(&root.path)
                                .ok()
                                .and_then(|p| p.to_str().map(str::to_owned))
                                .unwrap_or_else(|| root.path.clone()),
                        };
                        if !known_roots
                            .iter()
                            .any(|old| old.id == root.id && old.path == root.path)
                        {
                            known_roots.push(root.clone());
                        }
                    }
                    known_roots.retain(|root| paths.iter().any(|p| p.starts_with(&root.path)));
                    session.root_paths = known_roots.clone();
                    let result = database
                        .as_ref()
                        .filter(|_| writable)
                        .ok_or_else(|| storage("Session database is not open"))
                        .and_then(|db| save(db, &session));
                    (serial, result)
                }
            };
            publish(
                mailbox,
                match result {
                    Ok(()) => SessionUpdate::Saved { serial },
                    Err(error) => SessionUpdate::Error(error),
                },
            );
        }
        if stopping {
            break;
        }
        thread::park_timeout(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "auvra-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> String {
            self.0.to_str().unwrap().into()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn item(n: u32) -> MediaItem {
        MediaItem {
            id: MediaId::parse(format!("mock:track:{n}")).unwrap(),
            title: n.to_string(),
            artists: vec![],
            album: None,
            duration_ms: 8000,
            artwork: None,
            location: None,
            capabilities: Capabilities::DEMO,
        }
    }
    fn session() -> TransportSession {
        let mut queue = QueueEngine::default();
        queue
            .replace(
                (0..5).map(item).collect(),
                1,
                PlaybackContext::Search("quiet".into()),
            )
            .unwrap();
        queue.play_next(item(9)).unwrap();
        queue.next(false);
        queue.next(false);
        queue.previous();
        queue.play_next(item(8)).unwrap();
        queue.set_shuffle(true);
        queue.set_repeat(RepeatMode::All);
        TransportSession {
            remote: None,
            version: 1,
            queue,
            position_ms: 3456,
            volume: 0.34,
            stopped: false,
            root_paths: vec![],
        }
    }
    fn wait(
        worker: &mut SessionWorker,
        predicate: impl Fn(&SessionUpdate) -> bool,
    ) -> SessionUpdate {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(update) = worker.poll()
                && predicate(&update)
            {
                return update;
            }
            assert!(Instant::now() < deadline, "Session worker timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn worker(temp: &Temp) -> SessionWorker {
        let mut w = SessionWorker::new().unwrap();
        w.submit(SessionAction::Initialize {
            directory: temp.path(),
            access: SessionAccess::default(),
        })
        .unwrap();
        wait(&mut w, |u| matches!(u, SessionUpdate::Loaded { .. }));
        w
    }
    #[test]
    fn exact_queue_order_history_seed_settings_and_position_survive_reopen() {
        let temp = Temp::new();
        let original = session();
        {
            let db = open(&temp.path()).unwrap();
            save(&db, &original).unwrap();
        }
        let db = open(&temp.path()).unwrap();
        let mut restored = load(&db).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&original).unwrap(),
            serde_json::to_value(&restored).unwrap()
        );
        let mut expected = original.queue;
        for _ in 0..12 {
            assert_eq!(expected.next(false), restored.queue.next(false));
        }
        expected.set_shuffle(false);
        restored.queue.set_shuffle(false);
        for _ in 0..12 {
            assert_eq!(expected.next(false), restored.queue.next(false));
        }
    }
    fn remote_selection() -> RemoteSelection {
        let mut current = item(20);
        current.id = MediaId::parse("fixture:track:original").unwrap();
        current.capabilities = Capabilities::REMOTE;
        current.location = Some(MediaLocation::Remote {
            uri: current.id.as_str().into(),
            url: "https://example.invalid/original-track".into(),
            service: "Original fixture".into(),
        });
        RemoteSelection {
            provider: "fixture".into(),
            account_id: "original-account".into(),
            stable_account_id: None,
            current,
            position_ms: 3456,
            context_uri: Some("fixture:playlist:original".into()),
        }
    }
    #[test]
    fn legacy_remote_record_decodes_and_immutable_id_round_trips() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        let mut saved = remote_selection();
        let legacy = remote_payload(Some(&saved)).unwrap();
        assert!(!legacy.contains("stable_account_id"));
        db.execute("INSERT INTO remote_selection VALUES(1,?1)", [legacy])
            .unwrap();
        assert_eq!(load_remote(&db).unwrap(), Some(Some(saved.clone())));

        saved.stable_account_id = Some("original-stable-key".into());
        save_remote(&db, Some(&saved)).unwrap();
        assert_eq!(load_remote(&db).unwrap(), Some(Some(saved.clone())));
        let mut invalid = saved.clone();
        invalid.stable_account_id = Some("invalid\nkey".into());
        assert!(save_remote(&db, Some(&invalid)).is_err());
        assert_eq!(load_remote(&db).unwrap(), Some(Some(saved)));
    }
    #[test]
    fn version_one_loads_without_remote_data_and_version_two_preserves_the_local_archive() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        let original = session();
        let mut legacy = serde_json::to_value(&original).unwrap();
        legacy.as_object_mut().unwrap().remove("remote");
        db.execute(
            "INSERT INTO transport VALUES(1,?1)",
            [serde_json::to_string(&legacy).unwrap()],
        )
        .unwrap();
        let mut upgraded = load(&db).unwrap().unwrap();
        assert!(upgraded.remote.is_none());
        upgraded.version = 2;
        upgraded.remote = Some(remote_selection());
        save(&db, &upgraded).unwrap();
        let restored = load(&open(&temp.path()).unwrap()).unwrap().unwrap();
        assert_eq!(restored.remote, upgraded.remote);
        assert_eq!(restored.position_ms, original.position_ms);
        assert_eq!(
            serde_json::to_value(restored.queue).unwrap(),
            serde_json::to_value(original.queue).unwrap()
        );
    }
    #[test]
    fn chapter_identity_survives_remote_sqlite_round_trip_without_changing_local_transport() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        let mut original = session();
        let mut chapter = remote_selection();
        chapter.current.id = MediaId::parse("fixture:chapter:original-chapter").unwrap();
        if let Some(MediaLocation::Remote { uri, .. }) = chapter.current.location.as_mut() {
            *uri = chapter.current.id.as_str().into();
        }
        chapter.current.capabilities.library_save = false;
        original.version = 2;
        original.remote = Some(chapter);
        save(&db, &original).unwrap();
        let restored = load(&open(&temp.path()).unwrap()).unwrap().unwrap();
        assert_eq!(restored.remote, original.remote);
        assert_eq!(restored.position_ms, original.position_ms);
        assert_eq!(
            serde_json::to_value(restored.queue).unwrap(),
            serde_json::to_value(original.queue).unwrap()
        );
    }
    #[test]
    fn remote_record_rejects_local_pcm_leases_invalid_positions_and_expiring_artwork() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        let mut original = session();
        original.version = 2;
        original.remote = Some(remote_selection());
        save(&db, &original).unwrap();
        for invalid in 0..5 {
            let mut changed = original.clone();
            let remote = changed.remote.as_mut().unwrap();
            match invalid {
                0 => remote.current.capabilities.pcm_analysis = true,
                1 => remote.current.capabilities.system_media_session = true,
                2 => remote.position_ms = 9000,
                3 => remote.account_id.clear(),
                _ => {
                    remote.current.artwork =
                        Some("https://i.scdn.co/image/original-temporary".into())
                }
            }
            assert!(save(&db, &changed).is_err());
            assert_eq!(load(&db).unwrap().unwrap().remote, original.remote);
        }
    }
    #[test]
    fn latest_remote_checkpoint_is_drained_on_worker_shutdown() {
        let temp = Temp::new();
        let mut worker = worker(&temp);
        let mut saved = session();
        saved.version = 2;
        saved.remote = Some(remote_selection());
        worker
            .submit(SessionAction::Save {
                serial: 1,
                session: Box::new(saved.clone()),
            })
            .unwrap();
        saved.remote.as_mut().unwrap().position_ms = 6000;
        worker
            .submit(SessionAction::SaveRemote {
                serial: 2,
                selection: saved.remote.map(Box::new),
            })
            .unwrap();
        drop(worker);
        let restored = load(&open(&temp.path()).unwrap()).unwrap().unwrap();
        assert_eq!(restored.remote.unwrap().position_ms, 6000);
        assert_eq!(restored.position_ms, 3456);
        assert_eq!(restored.queue.current(), session().queue.current());
    }
    #[test]
    fn remote_checkpoints_do_not_write_the_local_archive() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        save(&db, &session()).unwrap();
        let before: String = db
            .query_row("SELECT payload FROM transport", [], |r| r.get(0))
            .unwrap();
        db.execute_batch("CREATE TRIGGER deny_local_insert BEFORE INSERT ON transport BEGIN SELECT RAISE(ABORT, 'local archive is immutable'); END; CREATE TRIGGER deny_local_update BEFORE UPDATE ON transport BEGIN SELECT RAISE(ABORT, 'local archive is immutable'); END;").unwrap();
        save_remote(&db, Some(&remote_selection())).unwrap();
        assert_eq!(load(&db).unwrap().unwrap().remote, Some(remote_selection()));
        save_remote(&db, None).unwrap();
        assert!(load(&db).unwrap().unwrap().remote.is_none());
        let after: String = db
            .query_row("SELECT payload FROM transport", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
    }
    #[test]
    fn full_archive_and_remote_marker_commit_together_or_roll_back_together() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        save(&db, &session()).unwrap();
        db.execute_batch("CREATE TRIGGER deny_remote_update BEFORE UPDATE ON remote_selection BEGIN SELECT RAISE(ABORT, 'remote storage rejected'); END;").unwrap();
        let mut changed = session();
        changed.version = 2;
        changed.position_ms = 1000;
        changed.remote = Some(remote_selection());
        assert!(save(&db, &changed).is_err());
        let restored = load(&db).unwrap().unwrap();
        assert_eq!(restored.position_ms, 3456);
        assert!(restored.remote.is_none());
    }
    #[test]
    fn legacy_combined_remote_is_read_but_a_local_tombstone_overrides_it() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        let mut legacy = session();
        legacy.version = 2;
        legacy.remote = Some(remote_selection());
        db.execute(
            "INSERT INTO transport VALUES(1,?1)",
            [serde_json::to_string(&legacy).unwrap()],
        )
        .unwrap();
        assert_eq!(load(&db).unwrap().unwrap().remote, legacy.remote);
        save_remote(&db, None).unwrap();
        let local = load(&db).unwrap().unwrap();
        assert!(local.remote.is_none());
        assert_eq!(local.position_ms, legacy.position_ms);
    }
    #[test]
    fn remote_only_database_restores_and_corrupt_remote_record_is_preserved() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        save_remote(&db, Some(&remote_selection())).unwrap();
        let restored = load(&db).unwrap().unwrap();
        assert_eq!(restored.remote, Some(remote_selection()));
        assert!(restored.queue.current().is_none());
        db.execute("UPDATE remote_selection SET payload='broken-remote'", [])
            .unwrap();
        let mut worker = SessionWorker::new().unwrap();
        worker
            .submit(SessionAction::Initialize {
                directory: temp.path(),
                access: SessionAccess::default(),
            })
            .unwrap();
        worker
            .submit(SessionAction::SaveRemote {
                serial: 1,
                selection: None,
            })
            .unwrap();
        wait(&mut worker, |u| matches!(u, SessionUpdate::Error(_)));
        drop(worker);
        let untouched: String = db
            .query_row("SELECT payload FROM remote_selection", [], |r| r.get(0))
            .unwrap();
        assert_eq!(untouched, "broken-remote");
    }
    #[test]
    fn invalid_and_newer_data_are_not_reset_or_overwritten_during_startup() {
        let temp = Temp::new();
        let db = open(&temp.path()).unwrap();
        db.execute("INSERT INTO transport VALUES(1,'invalid-json')", [])
            .unwrap();
        let mut w = SessionWorker::new().unwrap();
        w.submit(SessionAction::Initialize {
            directory: temp.path(),
            access: SessionAccess::default(),
        })
        .unwrap();
        w.submit(SessionAction::Save {
            serial: 1,
            session: Box::new(session()),
        })
        .unwrap();
        wait(&mut w, |u| matches!(u, SessionUpdate::Error(_)));
        drop(w);
        assert_eq!(
            db.query_row("SELECT payload FROM transport", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "invalid-json"
        );
        let mut invalid = serde_json::to_value(session()).unwrap();
        invalid["queue"]["remaining"] = serde_json::json!([999]);
        db.execute("UPDATE transport SET payload=?1", [invalid.to_string()])
            .unwrap();
        assert!(load(&db).is_err());
        db.pragma_update(None, "user_version", 99).unwrap();
        assert!(open(&temp.path()).is_err());
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            99
        );
    }
    #[test]
    fn latest_save_is_bounded_and_flushed_by_shutdown() {
        let temp = Temp::new();
        let mut w = worker(&temp);
        for n in 0..1000 {
            let mut s = session();
            s.position_ms = n;
            w.submit(SessionAction::Save {
                serial: n,
                session: Box::new(s),
            })
            .unwrap();
        }
        drop(w);
        assert_eq!(
            load(&open(&temp.path()).unwrap())
                .unwrap()
                .unwrap()
                .position_ms,
            999
        );
    }
    #[test]
    fn database_lock_reports_failure_and_subsequent_save_recovers() {
        let temp = Temp::new();
        let mut w = worker(&temp);
        let db = open(&temp.path()).unwrap();
        save(&db, &session()).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        w.submit(SessionAction::Save {
            serial: 1,
            session: Box::new(session()),
        })
        .unwrap();
        wait(&mut w, |u| matches!(u, SessionUpdate::Error(_)));
        db.execute_batch("ROLLBACK").unwrap();
        let mut latest = session();
        latest.position_ms = 4321;
        w.submit(SessionAction::Save {
            serial: 2,
            session: Box::new(latest),
        })
        .unwrap();
        wait(&mut w, |u| matches!(u, SessionUpdate::Saved { serial: 2 }));
        assert_eq!(load(&db).unwrap().unwrap().position_ms, 4321);
    }
    #[test]
    fn paths_are_not_grants_and_symlink_escape_is_rejected() {
        let temp = Temp::new();
        let music = temp.0.join("Music");
        let outside = temp.0.join("Music-outside");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let inside = music.join("inside.wav");
        let escape = outside.join("outside.wav");
        std::fs::write(&inside, []).unwrap();
        std::fs::write(&escape, []).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&escape, music.join("link.wav")).unwrap();
        let paths = [
            inside.clone(),
            escape.clone(),
            music.join("link.wav"),
            music.join("missing.wav"),
        ];
        let items: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(n, p)| {
                let mut i = item(n as u32);
                i.location = Some(MediaLocation::File {
                    path: p.to_str().unwrap().into(),
                });
                i
            })
            .collect();
        let mut q = QueueEngine::default();
        q.replace(items, 0, PlaybackContext::UserQueue).unwrap();
        assert_eq!(
            resolve(&mut q.clone(), &SessionAccess::default(), &[]).len(),
            4
        );
        let issues = resolve(
            &mut q,
            &SessionAccess {
                roots: vec![SessionRoot {
                    id: "music".into(),
                    path: music.to_str().unwrap().into(),
                }],
                files: vec![],
            },
            &[],
        );
        assert_eq!(issues.len(), 3);
        assert!(
            issues
                .iter()
                .any(|i| i.error.code == ErrorCode::FileMissing)
        );
    }
    #[test]
    fn restored_file_bookmark_can_rebase_a_moved_file_without_losing_queue_identity() {
        let temp = Temp::new();
        let old = temp.0.join("old.wav");
        let moved = temp.0.join("moved.wav");
        std::fs::write(&moved, []).unwrap();
        let mut i = item(0);
        i.location = Some(MediaLocation::File {
            path: old.to_str().unwrap().into(),
        });
        let mut q = QueueEngine::default();
        q.replace(vec![i.clone()], 0, PlaybackContext::UserQueue)
            .unwrap();
        assert!(
            resolve(
                &mut q,
                &SessionAccess {
                    roots: vec![],
                    files: vec![FileGrant {
                        original_path: old.to_str().unwrap().into(),
                        path: Some(moved.to_str().unwrap().into())
                    }]
                },
                &[]
            )
            .is_empty()
        );
        assert_eq!(q.current().unwrap().id, i.id);
        assert_eq!(
            q.current().unwrap().location,
            Some(MediaLocation::File {
                path: std::fs::canonicalize(moved)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into()
            })
        );
    }
    #[test]
    fn moved_folder_reconnect_rebases_the_full_queue_after_a_blocked_checkpoint() {
        let temp = Temp::new();
        let old = temp.0.join("Original Music");
        let moved = temp.0.join("Moved Music");
        std::fs::create_dir_all(&old).unwrap();
        let mut items = vec![];
        for n in 0..3 {
            let path = old.join(format!("{n}.wav"));
            std::fs::write(&path, []).unwrap();
            let mut i = item(n);
            i.location = Some(MediaLocation::File {
                path: path.to_str().unwrap().into(),
            });
            items.push(i);
        }
        let mut original = session();
        original
            .queue
            .replace(items, 0, PlaybackContext::LocalFolder("music".into()))
            .unwrap();
        original.root_paths = vec![SessionRoot {
            id: "music".into(),
            path: old.to_str().unwrap().into(),
        }];
        save(&open(&temp.path()).unwrap(), &original).unwrap();
        std::fs::rename(&old, &moved).unwrap();
        let mut w = SessionWorker::new().unwrap();
        w.submit(SessionAction::Initialize {
            directory: temp.path(),
            access: SessionAccess::default(),
        })
        .unwrap();
        let SessionUpdate::Loaded {
            session: Some(blocked),
            issues,
        } = wait(&mut w, |u| matches!(u, SessionUpdate::Loaded { .. }))
        else {
            panic!("session missing")
        };
        assert_eq!(issues.len(), 3);
        w.submit(SessionAction::Save {
            serial: 1,
            session: blocked.clone(),
        })
        .unwrap();
        wait(&mut w, |u| matches!(u, SessionUpdate::Saved { serial: 1 }));
        assert_eq!(
            load(&open(&temp.path()).unwrap())
                .unwrap()
                .unwrap()
                .root_paths
                .len(),
            1
        );
        w.submit(SessionAction::Validate {
            revision: blocked.queue.revision(),
            queue: Box::new(blocked.queue),
            access: SessionAccess {
                roots: vec![SessionRoot {
                    id: "music".into(),
                    path: moved.to_str().unwrap().into(),
                }],
                files: vec![],
            },
        })
        .unwrap();
        let SessionUpdate::Validated {
            mut queue, issues, ..
        } = wait(&mut w, |u| matches!(u, SessionUpdate::Validated { .. }))
        else {
            panic!("validation missing")
        };
        assert!(issues.is_empty());
        queue.visit_items_mut(|i| {
            if let Some(MediaLocation::File { path }) = &i.location {
                assert!(Path::new(path).starts_with(std::fs::canonicalize(&moved).unwrap()));
            }
        });
        let mut rebased = original;
        rebased.queue = *queue;
        w.submit(SessionAction::Save {
            serial: 2,
            session: Box::new(rebased),
        })
        .unwrap();
        drop(w);
        let saved = load(&open(&temp.path()).unwrap()).unwrap().unwrap();
        assert_eq!(saved.root_paths.len(), 1);
        assert_eq!(
            saved.root_paths[0].path,
            std::fs::canonicalize(moved).unwrap().to_str().unwrap()
        );
    }
}

#[cfg(test)]
mod sound_tests {
    use super::*;
    #[test]
    fn version_one_database_migrates_without_touching_transport() {
        let root =
            std::env::temp_dir().join(format!("auvra-sound-migration-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let db = Connection::open(root.join("transport.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE transport (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); INSERT INTO transport VALUES (1,'original untouched payload'); PRAGMA user_version=1;").unwrap();
        drop(db);
        let db = open(root.to_str().unwrap()).unwrap();
        let original: String = db
            .query_row("SELECT payload FROM transport", [], |r| r.get(0))
            .unwrap();
        assert_eq!(original, "original untouched payload");
        assert_eq!(load_sound(&db).unwrap(), SoundSettings::default());
        let mut settings = SoundSettings::default();
        settings.equalizer.enabled = true;
        settings.equalizer.bands_db[2] = 6.0;
        settings.presets.push(EqualizerPreset {
            name: "My voice".into(),
            equalizer: settings.equalizer,
        });
        save_sound(&db, &settings).unwrap();
        drop(db);
        let db = open(root.to_str().unwrap()).unwrap();
        assert_eq!(load_sound(&db).unwrap(), settings);
        db.execute("UPDATE sound_preferences SET payload='broken'", [])
            .unwrap();
        assert!(load_sound(&db).is_err());
        let damaged: String = db
            .query_row("SELECT payload FROM sound_preferences", [], |r| r.get(0))
            .unwrap();
        assert_eq!(damaged, "broken");
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
