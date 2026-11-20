use crate::{Database, error, scanner::Scanner};
use player_domain::*;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Inbox {
    mutations: VecDeque<LibraryAction>,
    query: Option<LibraryAction>,
    playback: Option<LibraryAction>,
}
#[derive(Default)]
struct Outbox {
    state: Option<LibraryState>,
    page: Option<LibraryPage>,
    playback: Option<LibraryUpdate>,
    errors: VecDeque<PlayerError>,
}
impl Outbox {
    fn publish(&mut self, update: LibraryUpdate) {
        match update {
            LibraryUpdate::State(s) => self.state = Some(s),
            LibraryUpdate::Page(p) => self.page = Some(p),
            p @ LibraryUpdate::Playback { .. } => self.playback = Some(p),
            LibraryUpdate::Error(e) => {
                if self.errors.len() == 8 {
                    self.errors.pop_front();
                }
                self.errors.push_back(e);
            }
        }
    }
    fn pop(&mut self) -> Option<LibraryUpdate> {
        self.state
            .take()
            .map(LibraryUpdate::State)
            .or_else(|| self.page.take().map(LibraryUpdate::Page))
            .or_else(|| self.playback.take())
            .or_else(|| self.errors.pop_front().map(LibraryUpdate::Error))
    }
}
#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    outbox: Mutex<Outbox>,
    stop: AtomicBool,
    epoch: AtomicU64,
}
impl Shared {
    fn publish(&self, u: LibraryUpdate) {
        self.outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(u);
    }
}
/// One serial owner submits non-blocking commands. SQLite and IO never run here.
pub struct LibraryWorker {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    wake: Thread,
}
impl LibraryWorker {
    pub fn new() -> Result<Self, PlayerError> {
        let shared = Arc::new(Shared::default());
        let child = shared.clone();
        let worker = thread::Builder::new()
            .name("auvra-library".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&child))).is_err() {
                    child.publish(LibraryUpdate::Error(error(
                        "Library worker stopped unexpectedly; reopen Auvra",
                    )));
                }
            })
            .map_err(error)?;
        let wake = worker.thread().clone();
        Ok(Self {
            shared,
            worker: Some(worker),
            wake,
        })
    }
}
impl LibraryProvider for LibraryWorker {
    fn submit(&mut self, action: LibraryAction) -> Result<(), PlayerError> {
        if self.worker.as_ref().is_none_or(|w| w.is_finished()) {
            return Err(error("Library worker is unavailable"));
        }
        validate(&action)?;
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        match action {
            action @ LibraryAction::Query(_) => inbox.query = Some(action),
            action @ LibraryAction::PreparePlayback { .. } => inbox.playback = Some(action),
            action => {
                if inbox.mutations.len() == 32 {
                    return Err(PlayerError::new(
                        ErrorCode::LibraryBusy,
                        "The library is busy; try again shortly",
                    ));
                }
                self.shared.epoch.fetch_add(1, Ordering::AcqRel);
                inbox.mutations.push_back(action);
            }
        }
        drop(inbox);
        self.wake.unpark();
        Ok(())
    }
    fn poll(&mut self) -> Option<LibraryUpdate> {
        self.shared
            .outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
    }
}
impl Drop for LibraryWorker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake.unpark();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn validate(action: &LibraryAction) -> Result<(), PlayerError> {
    match action {
        LibraryAction::Initialize { directory, roots } => {
            if directory.len() > 3500
                || directory.contains('\0')
                || !Path::new(directory).is_absolute()
                || roots.len() > MAX_LIBRARY_ROOTS
            {
                return Err(PlayerError::invalid("Invalid library configuration"));
            }
            for root in roots {
                root.validate()?;
            }
            let unique: std::collections::BTreeSet<_> = roots.iter().map(|r| &r.id).collect();
            if unique.len() != roots.len() {
                return Err(PlayerError::invalid("Duplicate folder ID"));
            }
        }
        LibraryAction::AddRoot(root) => root.validate()?,
        LibraryAction::Query(q) => q.validate()?,
        LibraryAction::PreparePlayback {
            query, track_id, ..
        } => {
            query.validate()?;
            if !(track_id.is_empty() && query.album_id.is_some()) {
                MediaId::parse(track_id.clone())?;
            }
        }
        LibraryAction::RemoveRoot(id) => {
            if id.len() > 64 {
                return Err(PlayerError::invalid("Invalid folder ID"));
            }
        }
        LibraryAction::Scan(id) => {
            if id.as_ref().is_some_and(|s| s.len() > 64) {
                return Err(PlayerError::invalid("Invalid folder ID"));
            }
        }
        LibraryAction::CancelScan => {}
    }
    Ok(())
}
fn normalize(mut root: AuthorizedRoot) -> AuthorizedRoot {
    if let Some(path) = &root.path
        && let Ok(path) = std::fs::canonicalize(path)
    {
        root.path = Some(path.to_string_lossy().into_owned());
    }
    root
}
struct Service {
    database: Option<Database>,
    directory: Option<String>,
    revision: u64,
    scan: Option<Scanner>,
    pending: VecDeque<String>,
    last_scan: Option<ScanProgress>,
}
impl Service {
    fn state(&self) -> Result<LibraryState, PlayerError> {
        let mut state = if let Some(db) = &self.database {
            db.state(self.revision)?
        } else {
            LibraryState::default()
        };
        state.scan = self.scan.as_ref().map(|s| s.progress.clone());
        state.last_scan = self.last_scan.clone();
        if self.scan.is_none() {
            for root in &mut state.roots {
                if root.status == "scanning" {
                    root.status = "partial".into();
                    root.issue = Some("Scan interrupted; rescan the folder".into());
                }
            }
        }
        Ok(state)
    }
    fn publish_state(&self, shared: &Shared) {
        match self.state() {
            Ok(state) => shared.publish(LibraryUpdate::State(state)),
            Err(e) => shared.publish(LibraryUpdate::State(LibraryState {
                issue: Some(e.message),
                ..Default::default()
            })),
        }
    }
    fn action(&mut self, action: LibraryAction, shared: &Shared) -> Result<(), PlayerError> {
        match action {
            LibraryAction::Initialize { directory, roots } => {
                if self.directory.as_ref().is_some_and(|d| d != &directory) {
                    return Err(error(
                        "A library handle cannot change its storage directory",
                    ));
                }
                if self.database.is_none() {
                    self.database = Some(Database::open(Path::new(&directory))?);
                    self.directory = Some(directory);
                }
                let roots: Vec<_> = roots.into_iter().map(normalize).collect();
                let db = self.database.as_mut().unwrap();
                db.initialize(&roots)?;
                self.pending = db
                    .roots()?
                    .into_iter()
                    .filter(|r| r.authorized)
                    .map(|r| r.id)
                    .collect();
                self.revision += 1;
            }
            action => {
                let db = self
                    .database
                    .as_mut()
                    .ok_or_else(|| error("The library has not been initialized"))?;
                match action {
                    LibraryAction::AddRoot(root) => {
                        let root = normalize(root);
                        db.add_root(&root)?;
                        self.pending.retain(|id| id != &root.id);
                        self.pending.push_back(root.id);
                        self.revision += 1;
                    }
                    LibraryAction::RemoveRoot(id) => {
                        self.pending.retain(|r| r != &id);
                        db.remove_root(&id)?;
                        db.prune_artwork()?;
                        self.revision += 1;
                    }
                    LibraryAction::Scan(id) => {
                        self.pending = match id {
                            Some(id) => VecDeque::from([id]),
                            None => db
                                .roots()?
                                .into_iter()
                                .filter(|r| r.authorized)
                                .map(|r| r.id)
                                .collect(),
                        };
                    }
                    LibraryAction::CancelScan => self.pending.clear(),
                    LibraryAction::Query(q) => {
                        shared.publish(LibraryUpdate::Page(db.query(&q, self.revision)?));
                        return Ok(());
                    }
                    LibraryAction::PreparePlayback {
                        token,
                        query,
                        track_id,
                    } => {
                        let (items, start) = db.playback(&query, &track_id)?;
                        shared.publish(LibraryUpdate::Playback {
                            token,
                            items,
                            start,
                            album_id: query.album_id,
                        });
                        return Ok(());
                    }
                    LibraryAction::Initialize { .. } => unreachable!(),
                }
            }
        }
        shared.publish(LibraryUpdate::State(self.state()?));
        Ok(())
    }
}
fn run(shared: &Shared) {
    let mut service = Service {
        database: None,
        directory: None,
        revision: 0,
        scan: None,
        pending: VecDeque::new(),
        last_scan: None,
    };
    let mut published = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        if service
            .scan
            .as_ref()
            .is_some_and(|s| s.epoch != shared.epoch.load(Ordering::Acquire))
        {
            let cancelled = service.scan.take().unwrap();
            if !service.pending.contains(&cancelled.progress.root_id) {
                service
                    .pending
                    .push_front(cancelled.progress.root_id.clone());
            }
            service.last_scan = Some(cancelled.progress.clone());
            if let Some(db) = &service.database {
                let _ = db.status(
                    &cancelled.progress.root_id,
                    "cancelled",
                    Some("Scan cancelled; previous catalog retained"),
                );
            }
            service.revision += 1;
        }
        let action = {
            let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox
                .mutations
                .pop_front()
                .or_else(|| inbox.query.take())
                .or_else(|| inbox.playback.take())
        };
        if let Some(action) = action {
            let mutation = !matches!(
                &action,
                LibraryAction::Query(_) | LibraryAction::PreparePlayback { .. }
            );
            if let Err(e) = service.action(action, shared) {
                shared.publish(LibraryUpdate::Error(e));
                if mutation {
                    service.publish_state(shared);
                }
            }
            continue;
        }
        if service.scan.is_none()
            && let Some(id) = service.pending.pop_front()
            && let Some(db) = &service.database
        {
            match db
                .roots()
                .and_then(|roots| {
                    roots
                        .into_iter()
                        .find(|r| r.id == id)
                        .ok_or_else(|| error("Folder no longer exists in the library"))
                })
                .and_then(|root| Scanner::start(&root, shared.epoch.load(Ordering::Acquire), db))
            {
                Ok(scan) => service.scan = Some(scan),
                Err(e) => {
                    let _ = db.status(&id, "unavailable", Some(&e.message));
                    service.revision += 1;
                    shared.publish(LibraryUpdate::Error(e));
                }
            }
            service.publish_state(shared);
        }
        if let (Some(scan), Some(db)) = (service.scan.as_mut(), service.database.as_mut()) {
            match scan.step(db) {
                Ok(true) => {
                    let finished = service.scan.take().unwrap();
                    match finished.finish(db) {
                        Ok(progress) => service.last_scan = Some(progress),
                        Err(e) => shared.publish(LibraryUpdate::Error(e)),
                    };
                    service.revision += 1;
                    let _ = db.prune_artwork();
                    published = Instant::now() - Duration::from_secs(1);
                }
                Ok(false) => {}
                Err(e) => {
                    let scan = service.scan.take().unwrap();
                    let _ = db.status(&scan.progress.root_id, "partial", Some(&e.message));
                    service.revision += 1;
                    shared.publish(LibraryUpdate::Error(e));
                    published = Instant::now() - Duration::from_secs(1);
                }
            }
            if published.elapsed() >= Duration::from_millis(200) {
                service.publish_state(shared);
                published = Instant::now();
            }
        } else {
            thread::park_timeout(Duration::from_millis(25));
        }
    }
}
