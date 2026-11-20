use crate::{FileFingerprint, metadata::ScannedTrack};
use player_domain::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn error(e: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(ErrorCode::DatabaseError, format!("Library database: {e}"))
}
pub struct Database {
    connection: Connection,
    pub(crate) cache: PathBuf,
}
fn number(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}
fn optional_number(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(index)?;
    value
        .map(|v| {
            u64::try_from(v).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })
        })
        .transpose()
}
fn integer(value: u64) -> Result<i64, PlayerError> {
    i64::try_from(value).map_err(error)
}
const VISIBLE: &str = "EXISTS(SELECT 1 FROM root_tracks rt JOIN roots r ON r.id=rt.root_id WHERE rt.track_id=t.id AND (?3 IS NULL OR rt.root_id=?3))";
const AVAILABLE: &str = "EXISTS(SELECT 1 FROM root_tracks ar JOIN roots gr ON gr.id=ar.root_id WHERE ar.track_id=t.id AND ar.present=1 AND gr.authorized=1 AND gr.status!='unavailable') AND t.issue IS NULL";
impl Database {
    pub fn open(directory: &Path) -> Result<Self, PlayerError> {
        std::fs::create_dir_all(directory).map_err(error)?;
        let mut connection = Connection::open(directory.join("library.sqlite3")).map_err(error)?;
        connection
            .busy_timeout(Duration::from_millis(500))
            .map_err(error)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
            )
            .map_err(error)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(error)?;
        if version > 1 {
            return Err(error("This library was created by a newer Auvra version"));
        }
        if version == 0 {
            let tx = connection.transaction().map_err(error)?;
            tx.execute_batch("CREATE TABLE roots(id TEXT PRIMARY KEY,name TEXT NOT NULL,path TEXT NOT NULL,authorized INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'needs_access',issue TEXT,last_scan_ms INTEGER);
            CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT UNIQUE NOT NULL,item_json TEXT NOT NULL,album_id TEXT NOT NULL,album_title TEXT NOT NULL,album_artist TEXT NOT NULL,year INTEGER,track_no INTEGER,disc_no INTEGER,duration_ms INTEGER NOT NULL,artwork_hash TEXT,size_bytes INTEGER NOT NULL,modified_ns INTEGER NOT NULL,issue TEXT,search_text TEXT NOT NULL);
            CREATE TABLE root_tracks(root_id TEXT NOT NULL REFERENCES roots(id) ON DELETE CASCADE,track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,present INTEGER NOT NULL,seen_scan INTEGER NOT NULL,PRIMARY KEY(root_id,track_id));
            CREATE INDEX membership_track ON root_tracks(track_id); CREATE INDEX album_tracks ON tracks(album_id,disc_no,track_no); CREATE INDEX track_title ON tracks(search_text); PRAGMA user_version=1;").map_err(error)?;
            tx.commit().map_err(error)?;
        }
        connection
            .execute_batch("CREATE INDEX IF NOT EXISTS tracks_artwork ON tracks(artwork_hash);")
            .map_err(error)?;
        Ok(Self {
            connection,
            cache: directory.join("artwork"),
        })
    }
    pub(crate) fn initialize(&mut self, roots: &[AuthorizedRoot]) -> Result<(), PlayerError> {
        let tx = self.connection.transaction().map_err(error)?;
        tx.execute("UPDATE roots SET authorized=0,status='needs_access',issue='Choose the folder to restore access'",[]).map_err(error)?;
        for root in roots {
            Self::write_root(&tx, root)?;
        }
        tx.commit().map_err(error)
    }
    fn write_root(connection: &Connection, root: &AuthorizedRoot) -> Result<(), PlayerError> {
        let count: u64 = connection
            .query_row("SELECT count(*) FROM roots WHERE id<>?1", [&root.id], |r| {
                number(r, 0)
            })
            .map_err(error)?;
        if count >= MAX_LIBRARY_ROOTS as u64 {
            return Err(error("Too many library folders"));
        }
        if let Some(path) = &root.path {
            let duplicate: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM roots WHERE path=?1 AND id<>?2)",
                    params![path, root.id],
                    |r| r.get(0),
                )
                .map_err(error)?;
            if duplicate {
                return Err(error("This folder is already in the library"));
            }
        }
        connection.execute("INSERT INTO roots(id,name,path,authorized,status,issue) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,path=CASE WHEN excluded.path='' THEN roots.path ELSE excluded.path END,authorized=excluded.authorized,status=excluded.status,issue=excluded.issue",params![root.id,root.name,root.path.as_deref().unwrap_or(""),root.path.is_some(),if root.path.is_some(){"ready"}else{"needs_access"},if root.path.is_some(){None}else{Some("Choose the folder to restore access")}]).map_err(error)?;
        Ok(())
    }
    pub(crate) fn add_root(&mut self, root: &AuthorizedRoot) -> Result<(), PlayerError> {
        Self::write_root(&self.connection, root)
    }
    pub(crate) fn remove_root(&mut self, id: &str) -> Result<(), PlayerError> {
        let tx = self.connection.transaction().map_err(error)?;
        tx.execute("DELETE FROM roots WHERE id=?1", [id])
            .map_err(error)?;
        tx.execute("DELETE FROM tracks WHERE NOT EXISTS(SELECT 1 FROM root_tracks WHERE track_id=tracks.id)",[]).map_err(error)?;
        tx.commit().map_err(error)
    }
    pub(crate) fn roots(&self) -> Result<Vec<LibraryRoot>, PlayerError> {
        let mut stmt=self.connection.prepare("SELECT r.id,r.name,r.path,r.authorized,r.status,r.issue,r.last_scan_ms,(SELECT count(*) FROM root_tracks WHERE root_id=r.id) FROM roots r ORDER BY lower(r.name),r.id").map_err(error)?;
        stmt.query_map([], |r| {
            Ok(LibraryRoot {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                authorized: r.get(3)?,
                status: r.get(4)?,
                issue: r.get(5)?,
                last_scan_ms: optional_number(r, 6)?,
                track_count: number(r, 7)?,
            })
        })
        .map_err(error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(error)
    }
    pub(crate) fn status(
        &self,
        id: &str,
        status: &str,
        issue: Option<&str>,
    ) -> Result<(), PlayerError> {
        self.connection
            .execute(
                "UPDATE roots SET status=?2,issue=?3 WHERE id=?1",
                params![id, status, issue],
            )
            .map_err(error)?;
        Ok(())
    }
    pub(crate) fn cached(
        &self,
        path: &str,
    ) -> Result<Option<(ScannedTrack, FileFingerprint)>, PlayerError> {
        let row=self.connection.query_row("SELECT item_json,album_id,album_title,album_artist,year,track_no,disc_no,artwork_hash,issue,size_bytes,modified_ns FROM tracks WHERE path=?1",[path],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<u32>>(4)?,r.get::<_,Option<u32>>(5)?,r.get::<_,Option<u32>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<String>>(8)?,number(r,9)?,number(r,10)?))).optional().map_err(error)?;
        row.map(|r| {
            Ok((
                ScannedTrack {
                    item: serde_json::from_str(&r.0).map_err(error)?,
                    album_id: r.1,
                    album_title: r.2,
                    album_artist: r.3,
                    year: r.4,
                    track_number: r.5,
                    disc_number: r.6,
                    artwork_hash: r.7,
                    issue: r.8,
                },
                FileFingerprint {
                    size_bytes: r.9,
                    modified_unix_ns: r.10,
                },
            ))
        })
        .transpose()
    }
    pub(crate) fn seen(&self, root: &str, track: &str, scan: u64) -> Result<(), PlayerError> {
        self.connection.execute("INSERT INTO root_tracks(root_id,track_id,present,seen_scan) VALUES(?1,?2,1,?3) ON CONFLICT(root_id,track_id) DO UPDATE SET present=1,seen_scan=excluded.seen_scan",params![root,track,integer(scan)?]).map_err(error)?;
        Ok(())
    }
    pub(crate) fn upsert(
        &mut self,
        track: &ScannedTrack,
        file: &FileFingerprint,
        root: &str,
        scan: u64,
    ) -> Result<(), PlayerError> {
        let Some(MediaLocation::File { path }) = &track.item.location else {
            return Err(error("Missing file locator"));
        };
        let search = format!(
            "{} {} {} {}",
            track.item.title,
            track.item.artists.join(" "),
            track.album_title,
            track.album_artist
        )
        .to_lowercase();
        let tx = self.connection.transaction().map_err(error)?;
        tx.execute("INSERT INTO tracks(id,path,item_json,album_id,album_title,album_artist,year,track_no,disc_no,duration_ms,artwork_hash,size_bytes,modified_ns,issue,search_text) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) ON CONFLICT(id) DO UPDATE SET item_json=excluded.item_json,album_id=excluded.album_id,album_title=excluded.album_title,album_artist=excluded.album_artist,year=excluded.year,track_no=excluded.track_no,disc_no=excluded.disc_no,duration_ms=excluded.duration_ms,artwork_hash=excluded.artwork_hash,size_bytes=excluded.size_bytes,modified_ns=excluded.modified_ns,issue=excluded.issue,search_text=excluded.search_text",params![track.item.id.as_str(),path,serde_json::to_string(&track.item).map_err(error)?,track.album_id,track.album_title,track.album_artist,track.year,track.track_number,track.disc_number,integer(track.item.duration_ms)?,track.artwork_hash,integer(file.size_bytes)?,integer(file.modified_unix_ns)?,track.issue,search]).map_err(error)?;
        tx.execute("INSERT INTO root_tracks(root_id,track_id,present,seen_scan) VALUES(?1,?2,1,?3) ON CONFLICT(root_id,track_id) DO UPDATE SET present=1,seen_scan=excluded.seen_scan",params![root,track.item.id.as_str(),integer(scan)?]).map_err(error)?;
        tx.commit().map_err(error)
    }
    pub(crate) fn finish_scan(
        &mut self,
        root: &str,
        scan: u64,
        complete: bool,
        issue: Option<&str>,
    ) -> Result<(), PlayerError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_millis()
            .min(i64::MAX as u128) as u64;
        let tx = self.connection.transaction().map_err(error)?;
        if complete {
            tx.execute(
                "UPDATE root_tracks SET present=0 WHERE root_id=?1 AND seen_scan<>?2",
                params![root, integer(scan)?],
            )
            .map_err(error)?;
        }
        tx.execute(
            "UPDATE roots SET status=?2,issue=?3,last_scan_ms=?4 WHERE id=?1",
            params![
                root,
                if complete { "ready" } else { "partial" },
                issue,
                integer(now)?
            ],
        )
        .map_err(error)?;
        tx.commit().map_err(error)
    }
    pub(crate) fn state(&self, revision: u64) -> Result<LibraryState, PlayerError> {
        let (total_tracks,total_albums)=self.connection.query_row("SELECT count(*),count(DISTINCT album_id) FROM tracks t WHERE EXISTS(SELECT 1 FROM root_tracks WHERE track_id=t.id)",[],|r|Ok((number(r,0)?,number(r,1)?))).map_err(error)?;
        Ok(LibraryState {
            revision,
            ready: true,
            roots: self.roots()?,
            total_tracks,
            total_albums,
            ..Default::default()
        })
    }
    fn filter() -> String {
        format!(
            "{VISIBLE} AND (?1='' OR instr(t.search_text,?1)>0) AND (?2 IS NULL OR t.album_id=?2)"
        )
    }
    pub fn query(&self, q: &LibraryQuery, revision: u64) -> Result<LibraryPage, PlayerError> {
        q.validate()?;
        let filter = Self::filter();
        let text = q.text.trim().to_lowercase();
        let mut page = LibraryPage {
            query_id: q.query_id,
            revision,
            offset: q.offset,
            total: 0,
            tracks: vec![],
            albums: vec![],
        };
        if q.mode == LibraryMode::Tracks {
            page.total = self
                .connection
                .query_row(
                    &format!("SELECT count(*) FROM tracks t WHERE {filter}"),
                    params![text, q.album_id, q.root_id],
                    |r| number(r, 0),
                )
                .map_err(error)?;
            let sql = format!(
                "SELECT t.item_json,t.album_id,t.track_no,t.disc_no,({AVAILABLE}),t.issue FROM tracks t WHERE {filter} ORDER BY CASE WHEN ?2 IS NULL THEN t.album_title END COLLATE NOCASE,t.album_artist COLLATE NOCASE,coalesce(t.disc_no,1),coalesce(t.track_no,2147483647),t.search_text,t.id LIMIT ?4 OFFSET ?5"
            );
            let mut stmt = self.connection.prepare(&sql).map_err(error)?;
            let rows = stmt
                .query_map(
                    params![text, q.album_id, q.root_id, q.limit, q.offset],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<u32>>(2)?,
                            r.get::<_, Option<u32>>(3)?,
                            r.get::<_, bool>(4)?,
                            r.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .map_err(error)?;
            for row in rows {
                let (json, album_id, track_number, disc_number, available, issue) =
                    row.map_err(error)?;
                let mut item: MediaItem = serde_json::from_str(&json).map_err(error)?;
                if !available {
                    item.capabilities = Capabilities::default();
                }
                page.tracks.push(LibraryTrack {
                    item,
                    album_id,
                    track_number,
                    disc_number,
                    available,
                    issue: if available {
                        None
                    } else {
                        issue.or_else(|| Some("File missing or folder access unavailable".into()))
                    },
                });
            }
        } else {
            page.total = self
                .connection
                .query_row(
                    &format!("SELECT count(DISTINCT t.album_id) FROM tracks t WHERE {filter}"),
                    params![text, q.album_id, q.root_id],
                    |r| number(r, 0),
                )
                .map_err(error)?;
            let sql = format!(
                "SELECT t.album_id,t.album_title,t.album_artist,max(t.year),max(t.artwork_hash),count(*),sum(CASE WHEN ({AVAILABLE}) THEN 1 ELSE 0 END),sum(t.duration_ms) FROM tracks t WHERE {filter} GROUP BY t.album_id ORDER BY t.album_title COLLATE NOCASE,t.album_artist COLLATE NOCASE,t.album_id LIMIT ?4 OFFSET ?5"
            );
            let mut stmt = self.connection.prepare(&sql).map_err(error)?;
            page.albums = stmt
                .query_map(
                    params![text, q.album_id, q.root_id, q.limit, q.offset],
                    |r| {
                        let hash: Option<String> = r.get(4)?;
                        Ok(LibraryAlbum {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            artist: r.get(2)?,
                            year: r.get(3)?,
                            artwork: hash
                                .map(|s| self.cache.join(s).to_string_lossy().into_owned()),
                            track_count: number(r, 5)?,
                            available_count: number(r, 6)?,
                            duration_ms: number(r, 7)?,
                        })
                    },
                )
                .map_err(error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(error)?;
        }
        Ok(page)
    }
    pub(crate) fn playback(
        &self,
        q: &LibraryQuery,
        id: &str,
    ) -> Result<(Vec<MediaItem>, usize), PlayerError> {
        let mut q = q.clone();
        q.mode = LibraryMode::Tracks;
        q.offset = 0;
        q.limit = MAX_LIBRARY_PAGE;
        let mut items = vec![];
        let mut bytes = 0usize;
        loop {
            let page = self.query(&q, 0)?;
            for track in page.tracks.into_iter().filter(|t| t.available) {
                bytes += serde_json::to_vec(&track.item).map_err(error)?.len();
                items.push(track.item);
            }
            if items.len() > 4096 || bytes > 8 * 1024 * 1024 {
                return Err(error(
                    "Choose an album or narrow the search before playing this collection",
                ));
            }
            q.offset = q.offset.saturating_add(q.limit);
            if u64::from(q.offset) >= page.total {
                break;
            }
        }
        let start = if id.is_empty() && !items.is_empty() {
            Some(0)
        } else {
            items.iter().position(|item| item.id.as_str() == id)
        }
        .ok_or_else(|| {
            PlayerError::new(
                ErrorCode::TrackUnavailable,
                "This library track is unavailable",
            )
        })?;
        Ok((items, start))
    }
    pub(crate) fn prune_artwork(&self) -> Result<(), PlayerError> {
        let Ok(entries) = std::fs::read_dir(&self.cache) else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry.map_err(error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.len() != 68
                || !(name.ends_with(".png") || name.ends_with(".jpg"))
                || !name[..64].bytes().all(|b| b.is_ascii_hexdigit())
            {
                continue;
            }
            let used: bool = self
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tracks WHERE artwork_hash=?1)",
                    [name],
                    |r| r.get(0),
                )
                .map_err(error)?;
            if !used && entry.file_type().map_err(error)?.is_file() {
                std::fs::remove_file(entry.path()).map_err(error)?;
            }
        }
        Ok(())
    }
}
