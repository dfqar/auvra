use crate::{
    RateLimit, ResumeDispatchPhase, Shared,
    autoplay_trace::{self, DiscoveryStage, ResumeAlbumProbe},
    catalog::{ContextPageGuard, context_page_changed},
    mapping,
};
use player_domain::*;
use reqwest::{
    Method, Url,
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, Instant},
};

pub(crate) struct Api {
    pub(crate) playlist_page: Option<crate::catalog::PlaylistPageGuard>,
    client: Client,
    base: Url,
    pub token: Option<String>,
    shared: Shared,
    pub generation: u64,
}
pub(crate) struct ApiFailure {
    pub error: PlayerError,
    pub status: u16,
    pub retry_after: Option<u64>,
    pub previous_response_timeout: bool,
}
pub(crate) struct QueueResponse {
    pub current: Option<MediaId>,
    pub upcoming: Vec<MediaItem>,
}
pub(super) struct AlbumTrackMembership {
    track_id: String,
    album_id: String,
    observed_at: Instant,
}
const ALBUM_MEMBERSHIP_EXPIRY: Duration = Duration::from_secs(120);
const ALBUM_MEMBERSHIP_LIMIT: usize = 24;

pub(crate) enum ControlAcceptance {
    Standard,
    ContextResume { dispatched_at: Instant },
}
enum Reply<'a> {
    Acknowledgement,
    Json { timeout: Option<Duration> },
    ResumeProbe(&'a mut ResumeProbe),
}

impl Reply<'_> {
    fn timeout(&self) -> Option<Duration> {
        match self {
            Self::Acknowledgement => None,
            Self::Json { timeout } => *timeout,
            Self::ResumeProbe(probe) => Some(probe.timeout),
        }
    }

    fn probe_failure(&mut self, result: &'static str) {
        if let Self::ResumeProbe(probe) = self {
            probe.failure = Some(result);
        }
    }
}

struct ResumeProbe {
    kind: &'static str,
    started_at: Instant,
    timeout: Duration,
    status: u16,
    failure: Option<&'static str>,
}

impl ResumeProbe {
    fn new(kind: &'static str, timeout: Duration) -> Self {
        Self {
            kind,
            started_at: Instant::now(),
            timeout,
            status: 0,
            failure: None,
        }
    }

    fn failed(self, error: &ApiFailure) {
        let result = self.failure.unwrap_or(match (self.status, error.status) {
            (_, 401) => "auth_required",
            (_, 499) => "cancelled",
            (0, 429) => "rate_limited",
            (0, _) => "request_failed",
            _ => "http_error",
        });
        self.finish(result, 0);
    }

    fn finish(self, result: &'static str, rows: usize) {
        if let Some(event) =
            autoplay_trace::Event::discovery(DiscoveryStage::ResumeAlbumProbe(ResumeAlbumProbe {
                probe: self.kind,
                duration_ms: self.started_at.elapsed().as_millis(),
                timeout_ms: self.timeout.as_millis(),
                status: self.status,
                result,
                rows: rows.min(50),
            }))
        {
            event.write();
        }
    }
}
pub(crate) fn retry_after_seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(
        value
            .bytes()
            .fold(0_u64, |n, b| {
                n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
            })
            .max(1),
    )
}
impl ApiFailure {
    pub(super) fn invalid(error: PlayerError) -> Self {
        Self {
            error,
            status: 400,
            retry_after: None,
            previous_response_timeout: false,
        }
    }
    fn context_changed() -> Self {
        Self {
            error: context_page_changed(),
            status: 409,
            retry_after: None,
            previous_response_timeout: false,
        }
    }
    fn selection_cancelled() -> Self {
        Self {
            error: PlayerError::invalid("Playback selection changed before starting"),
            status: 499,
            retry_after: None,
            previous_response_timeout: false,
        }
    }
    fn rate_limit(limit: RateLimit) -> Self {
        Self {
            error: limit.issue(),
            status: 429,
            retry_after: Some(limit.seconds),
            previous_response_timeout: false,
        }
    }
    pub(crate) fn network() -> Self {
        Self {
            error: PlayerError::new(
                ErrorCode::NetworkError,
                "Spotify could not be reached. Check the connection and try again.",
            ),
            status: 0,
            retry_after: None,
            previous_response_timeout: false,
        }
    }
}

fn developer_quota_exceeded(response: Response) -> bool {
    const MAX_BODY: u64 = 4096;
    if response.content_length().is_some_and(|len| len > MAX_BODY) {
        return false;
    }
    let mut data = Vec::new();
    response.take(MAX_BODY + 1).read_to_end(&mut data).is_ok()
        && data.len() as u64 <= MAX_BODY
        && serde_json::from_slice::<Value>(&data)
            .is_ok_and(|value| value["error"]["reason"].as_str() == Some("QUOTA_EXCEEDED"))
}
impl Api {
    pub fn new(base: String, shared: Shared) -> Result<Self, reqwest::Error> {
        Ok(Self {
            playlist_page: None,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .connect_timeout(Duration::from_secs(3))
                .redirect(Policy::none())
                .build()?,
            base: Url::parse(&base).expect("fixed API URL"),
            token: None,
            shared,
            generation: 0,
        })
    }
    pub(super) fn remember_track_album(&self, track: &str, detail: &Value) {
        if !mapping::valid_id(track) {
            return;
        }
        let uri = format!("spotify:track:{track}");
        let album = &detail["album"];
        let album_id = match album.get("uri") {
            None | Some(Value::Null) => album["id"].as_str(),
            Some(value) => value
                .as_str()
                .and_then(|uri| uri.strip_prefix("spotify:album:")),
        }
        .filter(|id| mapping::valid_id(id) && playable_album_item(detail, &uri, track));
        let mut mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.generation != self.generation
            || mailbox.authorization_revoked
            || mailbox.stopping
        {
            return;
        }
        let tracks = &mut mailbox.resume_album_tracks;
        // A new response can change availability or relink the requested track.
        tracks.retain(|entry| {
            entry.track_id != track && entry.observed_at.elapsed() < ALBUM_MEMBERSHIP_EXPIRY
        });
        if let Some(album_id) = album_id {
            if tracks.len() >= ALBUM_MEMBERSHIP_LIMIT {
                tracks.pop_front();
            }
            tracks.push_back(AlbumTrackMembership {
                track_id: track.to_owned(),
                album_id: album_id.to_owned(),
                observed_at: Instant::now(),
            });
        }
    }
    fn cached_album_resume_offset(&self, track: &str, album: &str, uri: &str) -> Option<Value> {
        let known = {
            let mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            mailbox.generation == self.generation
                && !mailbox.authorization_revoked
                && !mailbox.stopping
                && mailbox.resume_album_tracks.iter().any(|entry| {
                    entry.track_id == track
                        && entry.album_id == album
                        && entry.observed_at.elapsed() < ALBUM_MEMBERSHIP_EXPIRY
                })
        };
        if !known {
            return None;
        }
        if let Some(event) = autoplay_trace::Event::discovery(DiscoveryStage::ResumeAlbumCacheHit) {
            event.write();
        }
        Some(json!({"uri":uri}))
    }
    pub(crate) fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Option<Value>, ApiFailure> {
        self.call_response(method.clone(), path, query, body, method == Method::GET)
    }
    pub(crate) fn call_response(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        expects_json: bool,
    ) -> Result<Option<Value>, ApiFailure> {
        self.call_payload(method, path, query, body, None, expects_json)
    }
    pub(crate) fn call_payload(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        raw: Option<&str>,
        expects_json: bool,
    ) -> Result<Option<Value>, ApiFailure> {
        let reply = if expects_json {
            Reply::Json { timeout: None }
        } else {
            Reply::Acknowledgement
        };
        self.request(method, path, query, body, raw, reply)
    }
    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        raw: Option<&str>,
        mut reply: Reply<'_>,
    ) -> Result<Option<Value>, ApiFailure> {
        self.check_generation()?;
        if let Some(limit) = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rate(Instant::now())
        {
            return Err(ApiFailure::rate_limit(limit));
        }
        let mut url = self.base.join(path).map_err(|_| ApiFailure::network())?;
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        let needs_length = method == Method::PUT || method == Method::POST;
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(self.token.as_deref().unwrap_or_default());
        if let Some(timeout) = reply.timeout() {
            request = request.timeout(timeout);
        }
        if let Some(raw) = raw {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "image/jpeg")
                .body(raw.to_owned());
        } else if let Some(body) = body {
            request = request.json(&body);
        } else if needs_length {
            // Spotify's edge rejects bodyless control requests without explicit length (411).
            request = request
                .header(reqwest::header::CONTENT_LENGTH, 0)
                .body(Vec::new());
        }
        let response = request.send().map_err(|error| {
            reply.probe_failure(if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect_error"
            } else {
                "network_error"
            });
            let mut failure = ApiFailure::network();
            if path == "me/player/previous" && error.is_timeout() && !error.is_connect() {
                failure.previous_response_timeout = true;
                // Spotify can apply a mutation after the response deadline.
                failure.error = PlayerError::new(
                    ErrorCode::NetworkError,
                    "Spotify did not confirm the previous-track command.",
                );
            }
            failure
        })?;
        self.check_generation()?;
        let status = response.status().as_u16();
        if let Reply::ResumeProbe(probe) = &mut reply {
            probe.status = status;
        }
        if status == 204 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_seconds);
            if status == 429 {
                let seconds = retry_after.unwrap_or(30);
                let received_at = Instant::now();
                let mut limit = RateLimit {
                    seconds,
                    developer_quota: false,
                };
                {
                    let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    if box_.generation == self.generation {
                        // Stop admissions and cancel queued HTTP work as soon as the
                        // headers arrive, even if Spotify delays the error body.
                        box_.limit_requests(seconds, received_at);
                        limit.developer_quota = box_.developer_quota;
                    }
                }
                self.shared.1.notify_all();
                if !limit.developer_quota && developer_quota_exceeded(response) {
                    self.check_generation()?;
                    limit.developer_quota = true;
                    let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    // A body from an expired response cannot reclassify a later hold.
                    if box_.generation == self.generation
                        && received_at.elapsed() < Duration::from_secs(seconds)
                    {
                        box_.developer_quota = true;
                    }
                }
                self.check_generation()?;
                return Err(ApiFailure::rate_limit(limit));
            }
            let playback = path == "me/player" || path.starts_with("me/player/");
            let playlist = path.starts_with("playlists/") || path == "me/playlists";
            let (code, message) = match status {
                401 => (
                    ErrorCode::AuthenticationRequired,
                    "Spotify authorization expired. Reconnect your account.",
                ),
                403 => (
                    ErrorCode::PermissionLost,
                    "Spotify denied this operation. Check Premium, the app's allowed users and granted permissions.",
                ),
                404 if playback => (
                    ErrorCode::ProviderUnavailable,
                    "The selected playback device is unavailable. Reconnect playback in Auvra and try again.",
                ),
                404 => (
                    ErrorCode::ProviderUnavailable,
                    if playlist {
                        "This playlist could not be found or opened. Refresh your playlists."
                    } else {
                        "This item could not be found or opened. Refresh the results or try another item."
                    },
                ),
                400 => (
                    ErrorCode::InvalidCommand,
                    "Spotify could not accept this request. Refresh the current view and try again.",
                ),
                _ => (
                    ErrorCode::NetworkError,
                    "Spotify is temporarily unavailable. Try again shortly.",
                ),
            };
            return Err(ApiFailure {
                error: PlayerError::new(code, format!("{message} (HTTP {status})")),
                status,
                retry_after,
                previous_response_timeout: false,
            });
        }
        // Control endpoints acknowledge through status, never through JSON metadata.
        // A successful 200 acknowledgement need not contain a JSON document.
        if matches!(reply, Reply::Acknowledgement) {
            return Ok(None);
        }
        let mut data = Vec::new();
        response
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|error| {
                let timeout = error.kind() == std::io::ErrorKind::TimedOut
                    || error
                        .get_ref()
                        .and_then(|cause| cause.downcast_ref::<reqwest::Error>())
                        .is_some_and(reqwest::Error::is_timeout);
                reply.probe_failure(if timeout { "timeout" } else { "body_error" });
                ApiFailure::network()
            })?;
        if data.len() > 2 * 1024 * 1024 {
            reply.probe_failure("body_limit");
            return Err(ApiFailure::network());
        }
        if data.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&data).map(Some).map_err(|_| {
            reply.probe_failure("invalid_json");
            ApiFailure::network()
        })
    }
    pub(crate) fn catalog_intent_matches(&self, query_id: u64) -> bool {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest_query_id
            .is_none_or(|latest| latest == query_id)
    }
    pub(crate) fn clear_context_page(&self, query_id: u64) {
        let mut mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.generation == self.generation
            && mailbox
                .latest_query_id
                .is_none_or(|latest| latest == query_id)
        {
            mailbox.context_page = None;
        }
    }
    pub(crate) fn remember_context_page(&self, page: ContextPageGuard) {
        let mut mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.generation == self.generation
            && !mailbox.stopping
            && !mailbox.authorization_revoked
            && mailbox
                .latest_query_id
                .is_none_or(|latest| latest == page.query_id)
        {
            mailbox.context_page = Some(page);
        }
    }
    pub(crate) fn playlist_snapshot(
        &self,
        id: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<String>, ApiFailure> {
        let value = self.request(
            Method::GET,
            &format!("playlists/{id}"),
            &[("fields", "snapshot_id".into())],
            None,
            None,
            Reply::Json { timeout },
        )?;
        Ok(value
            .as_ref()
            .and_then(|v| v["snapshot_id"].as_str())
            .filter(|v| !v.is_empty() && v.len() <= 1024 && !v.chars().any(char::is_control))
            .map(str::to_owned))
    }
    fn check_generation(&self) -> Result<(), ApiFailure> {
        let mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.stopping || mailbox.generation != self.generation {
            return Err(ApiFailure {
                error: PlayerError::unavailable("spotify"),
                status: 499,
                retry_after: None,
                previous_response_timeout: false,
            });
        }
        if mailbox.authorization_revoked {
            return Err(ApiFailure {
                error: PlayerError::new(
                    ErrorCode::AuthenticationRequired,
                    "Reconnect your Spotify account.",
                ),
                status: 401,
                retry_after: None,
                previous_response_timeout: false,
            });
        }
        Ok(())
    }
    pub fn devices(&self) -> Result<Vec<PlaybackDevice>, ApiFailure> {
        let value = self
            .call(Method::GET, "me/player/devices", &[], None)?
            .unwrap_or(Value::Null);
        Ok(value["devices"]
            .as_array()
            .ok_or_else(ApiFailure::network)?
            .iter()
            .take(100)
            .filter_map(mapping::device)
            .collect())
    }
    pub fn playback(&self) -> Result<Option<RemotePlayback>, ApiFailure> {
        self.read_playback(None)
    }
    pub fn playback_during_owned_navigation(&self) -> Result<Option<RemotePlayback>, ApiFailure> {
        self.read_playback(Some(crate::output_clock::CONTROL_FRESHNESS))
    }
    fn read_playback(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Option<RemotePlayback>, ApiFailure> {
        let Some(value) = self.request(
            Method::GET,
            "me/player",
            &[("additional_types", "track,episode".into())],
            None,
            None,
            Reply::Json { timeout },
        )?
        else {
            return Ok(None);
        };
        if !value["is_playing"].is_boolean() || !value["device"].is_object() {
            return Err(ApiFailure::network());
        }
        Ok(Some(mapping::playback(&value)))
    }
    pub fn queue(&self) -> Result<QueueResponse, ApiFailure> {
        let queue = self
            .call(Method::GET, "me/player/queue", &[], None)?
            .ok_or_else(ApiFailure::network)?;
        let current = match queue.get("currently_playing") {
            Some(Value::Null) => None,
            Some(item) => Some(mapping::queue_anchor(item).ok_or_else(ApiFailure::network)?),
            None => return Err(ApiFailure::network()),
        };
        let upcoming = queue["queue"]
            .as_array()
            .ok_or_else(ApiFailure::network)?
            .iter()
            .take(19)
            .filter_map(mapping::track)
            .collect();
        Ok(QueueResponse { current, upcoming })
    }
    fn check_selection_intent(&self, selection: &RemoteControl) -> Result<(), ApiFailure> {
        self.check_generation()?;
        let mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox
            .controls
            .iter()
            .any(|next| crate::supersedes(&next.control, selection))
            || mailbox.resume_dispatch.as_ref().is_some_and(|resume| {
                resume.matches_control(selection) && resume.phase == ResumeDispatchPhase::Cancelled
            })
        {
            return Err(ApiFailure::selection_cancelled());
        }
        Ok(())
    }
    fn claim_resume_dispatch(&self, selection: &RemoteControl) -> Result<(), ApiFailure> {
        let mut mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if mailbox.generation != self.generation
            || mailbox.stopping
            || mailbox.authorization_revoked
            || mailbox
                .controls
                .iter()
                .any(|next| crate::supersedes(&next.control, selection))
        {
            return Err(ApiFailure::selection_cancelled());
        }
        let Some(resume) = mailbox.resume_dispatch.as_mut().filter(|resume| {
            resume.matches_control(selection) && resume.phase == ResumeDispatchPhase::Preparing
        }) else {
            return Err(ApiFailure::selection_cancelled());
        };
        // Claim the PUT under the same lock as cancellation, before any bytes
        // can leave. After this point even a missing response is too late.
        let operation = resume.operation;
        resume.phase = ResumeDispatchPhase::Dispatched;
        mailbox.record_trace(crate::autoplay_trace::Event::discovery(
            crate::autoplay_trace::DiscoveryStage::ResumePutClaimed { operation },
        ));
        Ok(())
    }
    fn check_direct_owned_start(&self, selection: &RemoteControl) -> Result<(), ApiFailure> {
        self.check_generation()?;
        let mut mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        let active_conflict = mailbox.direct_owned_start.as_ref().is_some_and(|start| {
            start.active_context_origin.is_some()
                && (!mailbox.may_dispatch_owned_handoff(Instant::now())
                    || mailbox.devices.as_ref().is_some_and(|devices| {
                        crate::output_clock::active_context_rows_conflict(devices, &start.device)
                    })
                    || mailbox.state.as_ref().is_some_and(|state| {
                        crate::output_clock::active_context_state_conflicts(state, &start.device)
                    }))
        });
        if active_conflict
            && let Some(origin) = mailbox
                .direct_owned_start
                .as_mut()
                .and_then(|start| start.active_context_origin.as_mut())
        {
            // Discovery may finish during a 404, or already be merged by this
            // worker before the projection consumes it. Both veto a fallback.
            origin.revoke();
        }
        if !mailbox.prefers_direct_owned_start(selection)
            || !mailbox.direct_owned_start.as_ref().is_some_and(|start| {
                start
                    .observed_at
                    .is_some_and(|at| at.elapsed() <= crate::output_clock::CONTROL_FRESHNESS)
                    && start
                        .active_context_origin
                        .as_ref()
                        .is_none_or(|origin| origin.permits_dispatch())
            })
            || mailbox.controls.iter().any(|next| {
                crate::supersedes(&next.control, selection)
                    || matches!(
                        next.control,
                        RemoteControl::Transfer { .. } | RemoteControl::PauseOnDevice { .. }
                    )
            })
        {
            return Err(ApiFailure {
                error: PlayerError::invalid(
                    "The local playback device or selection changed before starting",
                ),
                status: 499,
                retry_after: None,
                previous_response_timeout: false,
            });
        }
        Ok(())
    }
    fn check_context_row(
        &self,
        selection: &RemoteControl,
        context: &str,
        item: &str,
        position: u32,
        query_id: u64,
    ) -> Result<(), ApiFailure> {
        self.check_selection_intent(selection)?;
        let snapshot = {
            let mailbox = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            mailbox
                .context_page
                .as_ref()
                .filter(|page| {
                    mailbox.generation == self.generation
                        && mailbox
                            .latest_query_id
                            .is_none_or(|latest| latest == query_id)
                        && page.context == context
                        && page.query_id == query_id
                        && page
                            .rows
                            .iter()
                            .any(|(row, id)| *row == position && id.as_str() == item)
                })
                .ok_or_else(ApiFailure::context_changed)?
                .snapshot
                .clone()
        };
        if let Some(id) = context.strip_prefix("spotify:playlist:") {
            // The playback endpoint accepts only a position. Compare the
            // collection version, or read the exact row when no version was available.
            let valid = match snapshot {
                Some(expected) => {
                    self.playlist_snapshot(id, Some(Duration::from_millis(1500)))?
                        .as_deref()
                        == Some(expected.as_str())
                }
                None => self.playlist_row_matches(id, position, item)?,
            };
            if !valid {
                return Err(ApiFailure::context_changed());
            }
        }
        if !self.catalog_intent_matches(query_id) {
            return Err(ApiFailure::context_changed());
        }
        Ok(())
    }
    fn playlist_row_matches(&self, id: &str, position: u32, uri: &str) -> Result<bool, ApiFailure> {
        let Some(page) = self.request(
            Method::GET,
            &format!("playlists/{id}/items"),
            &[
                ("limit", "1".into()),
                ("offset", position.to_string()),
                ("additional_types", "track,episode".into()),
                ("fields", "items(item(uri,is_playable,is_local,restrictions),track(uri,is_playable,is_local,restrictions)),offset".into()),
            ],
            None,
            None,
            Reply::Json { timeout: Some(Duration::from_millis(1500)) },
        )? else {
            return Ok(false);
        };
        Ok(page["offset"].as_u64() == Some(u64::from(position))
            && page["items"].as_array().is_some_and(
                |items| matches!(items.as_slice(), [item] if playable_playlist_item(item, uri)),
            ))
    }
    fn resume_context_offset(
        &self,
        context: &str,
        uri: &str,
        resume: &RemoteControl,
    ) -> Result<Option<Value>, ApiFailure> {
        let playlist = context
            .strip_prefix("spotify:playlist:")
            .filter(|id| mapping::valid_id(id));
        let album = context
            .strip_prefix("spotify:album:")
            .filter(|id| mapping::valid_id(id))
            .zip(
                uri.strip_prefix("spotify:track:")
                    .filter(|id| mapping::valid_id(id)),
            );
        if let Some((album_id, track_id)) = album {
            self.check_selection_intent(resume)?;
            if let Some(offset) = self.cached_album_resume_offset(track_id, album_id, uri) {
                return Ok(Some(offset));
            }
        }
        let path = if let Some(id) = playlist {
            format!("playlists/{id}/items")
        } else if let Some((id, _)) = album {
            format!("albums/{id}/tracks")
        } else {
            return Ok(None);
        };
        // Queue and Autoplay entries can retain a context that does not contain
        // the current item. Keep the context only when its rows prove membership.
        let deadline = Instant::now() + Duration::from_millis(1500);
        let rows_deadline = if playlist.is_some() {
            deadline
        } else {
            // Leave time to verify the track's own album if the list is unavailable.
            deadline - Duration::from_millis(500)
        };
        for offset in [0, 50] {
            self.check_selection_intent(resume)?;
            let Some(timeout) = rows_deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let mut query = vec![("limit", "50".into()), ("offset", offset.to_string())];
            if playlist.is_some() {
                query.extend([
                    ("additional_types", "track,episode".into()),
                    ("fields", "items(item(uri,is_playable,is_local,restrictions),track(uri,is_playable,is_local,restrictions)),next,total,offset".into()),
                ]);
            }
            let mut probe = (album.is_some() && autoplay_trace::enabled())
                .then(|| ResumeProbe::new("album_rows", timeout));
            let page = match self.request(
                Method::GET,
                &path,
                &query,
                None,
                None,
                probe.as_mut().map_or(
                    Reply::Json {
                        timeout: Some(timeout),
                    },
                    Reply::ResumeProbe,
                ),
            ) {
                Ok(Some(page)) => page,
                Ok(None) => {
                    if let Some(probe) = probe {
                        probe.finish("empty_response", 0);
                    }
                    break;
                }
                Err(error) => {
                    if let Some(probe) = probe {
                        probe.failed(&error);
                    }
                    if matches!(error.status, 401 | 429 | 499) {
                        return Err(error);
                    }
                    break;
                }
            };
            let Some(items) = page["items"].as_array() else {
                if let Some(probe) = probe {
                    probe.finish("invalid_page", 0);
                }
                break;
            };
            let page_offset = page["offset"].as_u64();
            if page_offset.is_some_and(|value| value != offset) {
                if let Some(probe) = probe {
                    probe.finish("offset_mismatch", items.len());
                }
                break;
            }
            if let Some(index) = items.iter().take(50).position(|row| match album {
                Some((_, track)) => playable_album_item(row, uri, track),
                None => playable_playlist_item(row, uri),
            }) {
                if let Some(probe) = probe {
                    probe.finish("member", items.len());
                }
                return Ok(Some(if playlist.is_none() && page_offset == Some(offset) {
                    json!({"position": offset + index as u64})
                } else {
                    json!({"uri":uri})
                }));
            }
            if page["next"].as_str().is_none() {
                let total = page["total"].as_u64();
                if total.is_some_and(|total| {
                    total <= offset + 50 && total.saturating_sub(offset) == items.len() as u64
                }) || playlist.is_some() && total.is_none()
                {
                    if let Some(probe) = probe {
                        probe.finish("nonmember", items.len());
                    }
                    // A complete page proves the item is absent or unplayable.
                    return Ok(None);
                }
                if total.is_none_or(|total| total <= offset + 50) {
                    if let Some(probe) = probe {
                        probe.finish("no_match", items.len());
                    }
                    break;
                }
            }
            if let Some(probe) = probe {
                probe.finish("no_match", items.len());
            }
        }
        let Some((album_id, track_id)) = album else {
            return Ok(None);
        };
        self.check_selection_intent(resume)?;
        // The catalog worker may have finished the same detail while rows were loading.
        if let Some(offset) = self.cached_album_resume_offset(track_id, album_id, uri) {
            return Ok(Some(offset));
        }
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(None);
        };
        // The track's own album can prove membership independently of a failed
        // list read. It cannot prove an absolute row, especially on multiple discs.
        let mut probe =
            autoplay_trace::enabled().then(|| ResumeProbe::new("track_detail", timeout));
        let detail = match self.request(
            Method::GET,
            &format!("tracks/{track_id}"),
            &[],
            None,
            None,
            probe.as_mut().map_or(
                Reply::Json {
                    timeout: Some(timeout),
                },
                Reply::ResumeProbe,
            ),
        ) {
            Ok(Some(detail)) => detail,
            Ok(None) => {
                if let Some(probe) = probe {
                    probe.finish("empty_response", 0);
                }
                return Ok(None);
            }
            Err(error) => {
                if let Some(probe) = probe {
                    probe.failed(&error);
                }
                if matches!(error.status, 401 | 429 | 499) {
                    return Err(error);
                }
                return Ok(None);
            }
        };
        let member = matches_catalog_identity(&detail["album"], context, album_id)
            && playable_album_item(&detail, uri, track_id);
        if let Some(probe) = probe {
            probe.finish(if member { "member" } else { "nonmember" }, 1);
        }
        Ok(member.then(|| json!({"uri":uri})))
    }
    #[cfg(test)]
    pub fn control(&self, control: &RemoteControl, device: Option<&str>) -> Result<(), ApiFailure> {
        self.control_with_owned_start(control, device, false, false)
            .map(|_| ())
    }

    pub fn control_with_owned_start(
        &self,
        control: &RemoteControl,
        device: Option<&str>,
        direct_owned_start: bool,
        tracks_operation: bool,
    ) -> Result<ControlAcceptance, ApiFailure> {
        if control.operation_id() == Some(0) {
            return Err(ApiFailure {
                error: PlayerError::invalid("Invalid playback operation identity"),
                status: 400,
                retry_after: None,
                previous_response_timeout: false,
            });
        }
        let mut params = vec![];
        if let Some(device) = device {
            params.push(("device_id", device.to_owned()));
        }
        let mut body = None;
        let mut transfer_on_not_found = None;
        let mut resumes_context = false;
        let (method, path) = match control {
            RemoteControl::Play => (Method::PUT, "me/player/play"),
            RemoteControl::Pause => (Method::PUT, "me/player/pause"),
            RemoteControl::PauseOnDevice { device_id } => {
                if !crate::valid_device_id(device_id) {
                    return Err(ApiFailure {
                        error: PlayerError::invalid("Invalid playback device"),
                        status: 400,
                        retry_after: None,
                        previous_response_timeout: false,
                    });
                }
                params.clear();
                params.push(("device_id", device_id.clone()));
                (Method::PUT, "me/player/pause")
            }
            RemoteControl::Next => (Method::POST, "me/player/next"),
            RemoteControl::Previous => (Method::POST, "me/player/previous"),
            RemoteControl::Seek { position_ms } => {
                params.push(("position_ms", position_ms.to_string()));
                (Method::PUT, "me/player/seek")
            }
            RemoteControl::Volume { volume } => {
                params.push((
                    "volume_percent",
                    ((volume * 100.0).round() as u8).to_string(),
                ));
                (Method::PUT, "me/player/volume")
            }
            RemoteControl::Shuffle { enabled } => {
                params.push(("state", enabled.to_string()));
                (Method::PUT, "me/player/shuffle")
            }
            RemoteControl::Repeat { mode } => {
                params.push((
                    "state",
                    match mode {
                        RepeatMode::Off => "off",
                        RepeatMode::One => "track",
                        RepeatMode::All => "context",
                    }
                    .into(),
                ));
                (Method::PUT, "me/player/repeat")
            }
            RemoteControl::Transfer { device_id, .. } => {
                params.clear();
                body = Some(json!({"device_ids":[device_id]}));
                (Method::PUT, "me/player")
            }
            RemoteControl::Start { item } => {
                body = Some(
                    json!({"uris":[mapping::track_uri(item).map_err(|error| ApiFailure {error,status:400,retry_after:None,previous_response_timeout:false})?]}),
                );
                (Method::PUT, "me/player/play")
            }
            RemoteControl::StartSpokenOnDevice {
                item,
                device_id,
                position_ms,
                ..
            } => {
                if !crate::valid_device_id(device_id) {
                    return Err(ApiFailure {
                        error: PlayerError::invalid("Invalid playback device"),
                        status: 400,
                        retry_after: None,
                        previous_response_timeout: false,
                    });
                }
                let uri =
                    mapping::spoken_start_uri(item, *position_ms).map_err(|error| ApiFailure {
                        error,
                        status: 400,
                        retry_after: None,
                        previous_response_timeout: false,
                    })?;
                params.clear();
                params.push(("device_id", device_id.clone()));
                body = Some(json!({"uris":[uri],"position_ms":position_ms}));
                (Method::PUT, "me/player/play")
            }
            RemoteControl::StartOnDevice {
                item, device_id, ..
            }
            | RemoteControl::ResumeOnDevice {
                item, device_id, ..
            } => {
                let uri = mapping::track_uri(item).map_err(|error| ApiFailure {
                    error,
                    status: 400,
                    retry_after: None,
                    previous_response_timeout: false,
                })?;
                if let RemoteControl::ResumeOnDevice {
                    position_ms,
                    context_uri,
                    ..
                } = control
                    && (*position_ms > item.duration_ms
                        || !crate::valid_device_id(device_id)
                        || context_uri
                            .as_deref()
                            .is_some_and(|uri| !mapping::valid_uri(uri)))
                {
                    return Err(ApiFailure {
                        error: PlayerError::invalid("Invalid saved playback position or context"),
                        status: 400,
                        retry_after: None,
                        previous_response_timeout: false,
                    });
                }
                // A preliminary transfer can move unrelated current playback to this
                // device. Exact Resume already targets the saved item and position.
                if matches!(control, RemoteControl::StartOnDevice { .. }) {
                    if direct_owned_start {
                        transfer_on_not_found = Some(device_id);
                    } else if device != Some(device_id.as_str()) {
                        self.call(
                            Method::PUT,
                            "me/player",
                            &[],
                            Some(json!({"device_ids":[device_id],"play":false})),
                        )?;
                    }
                }
                params.clear();
                params.push(("device_id", device_id.clone()));
                body = Some(json!({"uris":[uri]}));
                if let RemoteControl::ResumeOnDevice {
                    position_ms,
                    context_uri,
                    ..
                } = control
                {
                    let offset = match context_uri {
                        Some(context) => self.resume_context_offset(context, &uri, control)?,
                        None => None,
                    };
                    resumes_context = offset.is_some();
                    let plan = match (&offset, context_uri) {
                        (Some(offset), _) if offset.get("position").is_some() => "album_row",
                        (Some(_), Some(context)) if context.starts_with("spotify:album:") => {
                            "album_uri"
                        }
                        (Some(_), _) => "playlist_uri",
                        (None, Some(context)) if context.starts_with("spotify:album:") => {
                            "album_unverified"
                        }
                        (None, Some(context)) if context.starts_with("spotify:playlist:") => {
                            "playlist_unverified"
                        }
                        (None, Some(_)) => "unsupported_context",
                        (None, None) => "no_context",
                    };
                    if let Some(trace) = crate::autoplay_trace::Event::transport(
                        control,
                        crate::transport_trace::Stage::ResumeTarget(plan),
                        device == Some(device_id.as_str()),
                    ) {
                        trace.write();
                    }
                    body = Some(match offset {
                        Some(offset) => {
                            json!({"context_uri":context_uri,"offset":offset,"position_ms":position_ms})
                        }
                        None => json!({"uris":[uri],"position_ms":position_ms}),
                    });
                }
                (Method::PUT, "me/player/play")
            }
            RemoteControl::StartContext {
                uri,
                device_id,
                item,
                position,
                query_id,
                ..
            } => {
                let offset = mapping::context_start_offset(uri, device_id, item.as_ref())
                    .map_err(ApiFailure::invalid)?;
                let row = mapping::context_start_position(*position, *query_id, item.as_ref())
                    .map_err(ApiFailure::invalid)?;
                if offset.is_none() {
                    if direct_owned_start {
                        transfer_on_not_found = Some(device_id);
                    } else if device != Some(device_id.as_str()) {
                        self.call(
                            Method::PUT,
                            "me/player",
                            &[],
                            Some(json!({"device_ids":[device_id],"play":false})),
                        )?;
                    }
                }
                params.clear();
                params.push(("device_id", device_id.clone()));
                body = Some(match offset {
                    Some(track) => {
                        let offset = if let Some((position, query_id)) = row {
                            self.check_context_row(control, uri, &track, position, query_id)?;
                            json!({"position":position})
                        } else {
                            json!({"uri":track})
                        };
                        json!({"context_uri":uri,"offset":offset,"position_ms":0})
                    }
                    None => json!({"context_uri":uri}),
                });
                (Method::PUT, "me/player/play")
            }
            RemoteControl::Enqueue { item } => {
                params.push((
                    "uri",
                    mapping::queue_uri(item).map_err(|error| ApiFailure {
                        error,
                        status: 400,
                        retry_after: None,
                        previous_response_timeout: false,
                    })?,
                ));
                (Method::POST, "me/player/queue")
            }
            RemoteControl::Stop => {
                return Err(ApiFailure {
                    error: PlayerError::new(
                        ErrorCode::UnsupportedCapability,
                        "Spotify has no Stop command",
                    ),
                    status: 400,
                    retry_after: None,
                    previous_response_timeout: false,
                });
            }
        };
        if matches!(
            control,
            RemoteControl::ResumeOnDevice { .. }
                | RemoteControl::StartContext { item: Some(_), .. }
        ) {
            self.check_selection_intent(control)?;
        }
        if direct_owned_start {
            self.check_direct_owned_start(control)?;
        }
        if let Some(device_id) = transfer_on_not_found {
            // A targeted Play can select the owned output without
            // transferring and briefly playing the previous selection.
            match self.call(method.clone(), path, &params, body.clone()) {
                Err(error) if error.status == 404 => {
                    self.check_direct_owned_start(control)?;
                    self.call(
                        Method::PUT,
                        "me/player",
                        &[],
                        Some(json!({"device_ids":[device_id],"play":false})),
                    )?;
                    self.check_direct_owned_start(control)?;
                }
                result => return result.map(|_| ControlAcceptance::Standard),
            }
        }
        if tracks_operation && matches!(control, RemoteControl::ResumeOnDevice { .. }) {
            self.claim_resume_dispatch(control)?;
        }
        let acceptance = if resumes_context {
            ControlAcceptance::ContextResume {
                dispatched_at: Instant::now(),
            }
        } else {
            ControlAcceptance::Standard
        };
        match self.call(method, path, &params, body) {
            Err(error)
                if error.status == 404
                    && matches!(control, RemoteControl::PauseOnDevice { .. }) =>
            {
                Ok(ControlAcceptance::Standard)
            }
            result => result.map(|_| acceptance),
        }
    }
}

fn playable_playlist_item(row: &Value, uri: &str) -> bool {
    let item = row
        .get("item")
        .or_else(|| row.get("track"))
        .unwrap_or(&Value::Null);
    item["uri"].as_str() == Some(uri) && context_item_is_available(item)
}

fn matches_catalog_identity(item: &Value, uri: &str, id: &str) -> bool {
    match item.get("uri") {
        None | Some(Value::Null) => item["id"].as_str() == Some(id),
        Some(value) => value.as_str() == Some(uri),
    }
}

fn playable_album_item(item: &Value, uri: &str, id: &str) -> bool {
    matches_catalog_identity(item, uri, id) && context_item_is_available(item)
}

fn context_item_is_available(item: &Value) -> bool {
    item["is_local"].as_bool() != Some(true)
        && item["is_playable"].as_bool() != Some(false)
        && item["restrictions"]
            .as_object()
            .is_none_or(|r| r.is_empty())
}
