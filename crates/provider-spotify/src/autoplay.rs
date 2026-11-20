//! Catalog-based fallback for an explicitly selected track or album. Soloist
//! remains authoritative for the queue, repeat settings and playback.
use super::{
    api::{Api, ApiFailure},
    mapping,
};
use player_domain::*;
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const PAGE_SIZE: u32 = 10;
const MAX_OFFSET: u32 = 1000;
const RECENT_LIMIT: usize = 12;
const EMPTY_QUEUE_FRESHNESS: Duration = Duration::from_millis(1_200);
const END_WINDOW_MS: u64 = 1_400;
const SELECTION_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone)]
enum Seed {
    Track(MediaId),
    Album(MediaId),
}
impl Seed {
    fn id(&self) -> &MediaId {
        match self {
            Self::Track(id) | Self::Album(id) => id,
        }
    }

    fn matches(&self, clock: &RemoteOutputClock) -> bool {
        match self {
            Self::Track(id) => clock.media_id.as_ref() == Some(id),
            Self::Album(id) => {
                clock
                    .metadata
                    .as_ref()
                    .and_then(|item| item.context_uri.as_deref())
                    == Some(id.as_str())
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Artist {
    id: String,
    name: String,
}
fn artists(value: &Value) -> impl Iterator<Item = Artist> + '_ {
    value.as_array().into_iter().flatten().filter_map(|artist| {
        let id = artist["id"].as_str().filter(|id| mapping::valid_id(id))?;
        let name = artist["name"].as_str().filter(|name| {
            !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
        })?;
        Some(Artist {
            id: id.into(),
            name: name.into(),
        })
    })
}
fn year(value: &Value) -> Option<u16> {
    value["release_date"].as_str()?.get(..4)?.parse().ok()
}
fn title_family(title: &str) -> String {
    let title = title.split_once(" - ").map_or(title, |(name, _)| name);
    title
        .split_once(" (")
        .map_or(title, |(name, _)| name)
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Clone)]
struct Profile {
    seed_artists: Vec<Artist>,
    collaborators: Vec<Artist>,
    year: Option<u16>,
    title: Option<String>,
}
#[derive(Clone, Copy, Default)]
struct PageCursor {
    artist: usize,
    offsets: [u32; 8],
}
pub(super) struct Request {
    ticket: u64,
    seed: Seed,
    profile: Option<Profile>,
    cursor: PageCursor,
}
pub(super) struct Page {
    ticket: u64,
    profile: Profile,
    candidates: Vec<MediaId>,
    cursor: PageCursor,
}
impl Api {
    pub(super) fn autoplay_page(&self, request: Request) -> Result<Page, ApiFailure> {
        let mut profile = if let Some(profile) = request.profile {
            profile
        } else {
            let uri = request.seed.id().as_str();
            let kind = match &request.seed {
                Seed::Track(_) => "tracks",
                Seed::Album(_) => "albums",
            };
            let id = uri.rsplit_once(':').expect("validated radio seed URI").1;
            let entity = self
                .call(Method::GET, &format!("{kind}/{id}"), &[], None)?
                .unwrap_or(Value::Null);
            if entity["id"].as_str() != Some(id) {
                return Err(ApiFailure::network());
            }
            let seed_artists = artists(&entity["artists"]).take(4).collect::<Vec<_>>();
            if seed_artists.is_empty() {
                return Err(ApiFailure::network());
            }
            let (year, title) = match &request.seed {
                Seed::Track(_) => (
                    year(&entity["album"]),
                    entity["name"].as_str().map(title_family),
                ),
                Seed::Album(_) => (year(&entity), None),
            };
            Profile {
                seed_artists,
                collaborators: vec![],
                year,
                title,
            }
        };
        let artist = request.cursor.artist;
        let offset = request.cursor.offsets[artist];
        let anchor = profile
            .seed_artists
            .iter()
            .chain(&profile.collaborators)
            .nth(artist)
            .expect("cursor matches available radio artists")
            .clone();
        // Preserve field filter boundaries for artist names that contain quotes.
        let query = format!("artist:\"{}\"", anchor.name.replace(['"', '\\'], " "));
        let response = self
            .call(
                Method::GET,
                "search",
                &[
                    ("q", query),
                    ("type", "track".into()),
                    ("limit", PAGE_SIZE.to_string()),
                    ("offset", offset.to_string()),
                ],
                None,
            )?
            .unwrap_or(Value::Null);
        let tracks = &response["tracks"];
        let mut scored = Vec::new();
        for row in tracks["items"]
            .as_array()
            .into_iter()
            .flatten()
            .take(PAGE_SIZE as usize)
        {
            let Some(item) = mapping::track(row).filter(|item| {
                item.capabilities.play
                    && item.duration_ms > 0
                    && item.id.as_str().starts_with("spotify:track:")
                    && &item.id != request.seed.id()
                    && profile
                        .title
                        .as_ref()
                        .is_none_or(|title| title != &title_family(&item.title))
            }) else {
                continue;
            };
            if let Seed::Album(id) = &request.seed
                && row["album"]["id"].as_str() == id.as_str().strip_prefix("spotify:album:")
            {
                continue;
            }
            let row_artists = artists(&row["artists"]).take(8).collect::<Vec<_>>();
            if !row_artists.iter().any(|artist| artist.id == anchor.id) {
                continue;
            }
            let matches = row_artists
                .iter()
                .filter(|artist| {
                    profile
                        .seed_artists
                        .iter()
                        .any(|seed_artist| seed_artist.id == artist.id)
                })
                .count();
            if matches > 0 {
                for artist in &row_artists {
                    if profile.collaborators.len() == 4 {
                        break;
                    }
                    if !profile
                        .seed_artists
                        .iter()
                        .chain(&profile.collaborators)
                        .any(|other| other.id == artist.id)
                    {
                        profile.collaborators.push(artist.clone());
                    }
                }
            }
            let distance = year(&row["album"])
                .zip(profile.year)
                .map_or(100, |(row, seed)| row.abs_diff(seed));
            scored.push((std::cmp::Reverse(matches), distance, item.id));
        }
        scored.sort_by_key(|(matches, distance, _)| (*matches, *distance));
        let has_next = tracks["next"].is_string()
            && tracks["total"]
                .as_u64()
                .is_none_or(|total| u64::from(offset + PAGE_SIZE) < total)
            && offset + PAGE_SIZE <= MAX_OFFSET;
        let mut cursor = request.cursor;
        cursor.artist = (artist + 1) % (profile.seed_artists.len() + profile.collaborators.len());
        cursor.offsets[artist] = if has_next { offset + PAGE_SIZE } else { 0 };
        let mut candidates = Vec::with_capacity(scored.len());
        for (_, _, id) in scored {
            if !candidates.contains(&id) {
                candidates.push(id);
            }
        }
        Ok(Page {
            ticket: request.ticket,
            profile,
            cursor,
            candidates,
        })
    }
}

struct Selection {
    seed: Seed,
    device: Option<String>,
    selected_at: Instant,
    generation: Option<u64>,
    current: Option<MediaId>,
    restored_track: Option<MediaId>,
    context: Option<Option<String>>,
    native_next: Option<MediaId>,
    offered: Option<MediaId>,
    accepted: bool,
    attempted: bool,
    recent: VecDeque<MediaId>,
    candidates: VecDeque<MediaId>,
    recycle: VecDeque<MediaId>,
    cursor: PageCursor,
    profile: Option<Profile>,
    ticket: Option<u64>,
    fetch_after: Instant,
}
#[derive(Default)]
pub(super) struct Autoplay {
    selection: Option<Selection>,
    serial: u64,
}
impl Autoplay {
    pub(super) fn clear(&mut self) {
        self.selection = None;
    }

    pub(super) fn lose_binding(&mut self) {
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.generation.is_some())
        {
            self.clear();
        }
    }

    pub(super) fn control(&mut self, control: &RemoteControl, now: Instant) {
        let music = |item: &MediaItem| {
            item.id
                .as_str()
                .starts_with("spotify:track:")
                .then(|| Seed::Track(item.id.clone()))
        };
        let album = |uri: &str| {
            uri.strip_prefix("spotify:album:")
                .filter(|id| mapping::valid_id(id))?;
            MediaId::parse(uri).ok().map(Seed::Album)
        };
        let (seed, device) = match control {
            RemoteControl::Start { item } => (music(item), None),
            RemoteControl::StartOnDevice {
                item, device_id, ..
            } => (music(item), Some(device_id.clone())),
            RemoteControl::StartContext { uri, device_id, .. } => {
                (album(uri), Some(device_id.clone()))
            }
            RemoteControl::ResumeOnDevice {
                item,
                device_id,
                context_uri,
                ..
            } => {
                let seed = match context_uri.as_deref() {
                    Some(uri) if uri != item.id.as_str() => album(uri),
                    _ => music(item),
                };
                (seed, Some(device_id.clone()))
            }
            RemoteControl::Transfer { .. }
            | RemoteControl::StartSpokenOnDevice { .. }
            | RemoteControl::Stop
            | RemoteControl::PauseOnDevice { .. } => (None, None),
            _ => return,
        };
        self.selection = seed.map(|seed| Selection {
            seed,
            device,
            selected_at: now,
            generation: None,
            current: None,
            restored_track: match control {
                RemoteControl::ResumeOnDevice { item, .. } => Some(item.id.clone()),
                _ => None,
            },
            context: None,
            native_next: None,
            offered: None,
            accepted: false,
            attempted: false,
            recent: VecDeque::new(),
            candidates: VecDeque::new(),
            recycle: VecDeque::new(),
            cursor: PageCursor::default(),
            profile: None,
            ticket: None,
            fetch_after: now,
        });
    }

    pub(super) fn observe(
        &mut self,
        clock: &RemoteOutputClock,
        queue: Option<(&RemoteOutputQueue, Duration)>,
        now: Instant,
    ) {
        let Some(selection) = &mut self.selection else {
            return;
        };
        let Some(media) = clock.media_id.as_ref() else {
            return;
        };
        let wrong_device = selection
            .device
            .as_ref()
            .is_some_and(|device| *device != clock.device_id);
        let matches_selection =
            selection.seed.matches(clock) || selection.restored_track.as_ref() == Some(media);
        if let Some(generation) = selection.generation {
            if wrong_device || generation != clock.generation {
                self.clear();
                return;
            }
        } else if now.saturating_duration_since(selection.selected_at) > SELECTION_TIMEOUT {
            self.clear();
            return;
        } else if wrong_device || !matches_selection || clock.state != PlaybackState::Playing {
            return;
        } else {
            selection.generation = Some(clock.generation);
            selection.device = Some(clock.device_id.clone());
        }

        if selection.current.as_ref() != Some(media) {
            if !matches_selection
                && selection.native_next.as_ref() != Some(media)
                && selection.offered.as_ref() != Some(media)
            {
                // A granular transition precedes a full context observation.
                if clock.metadata.is_some() && clock.options.is_some() {
                    self.clear();
                }
                return;
            }
            if !media.as_str().starts_with("spotify:track:") {
                self.clear();
                return;
            }
            selection.current = Some(media.clone());
            selection.restored_track = None;
            selection.context = None;
            selection.native_next = None;
            selection.offered = None;
            selection.accepted = false;
            selection.attempted = false;
            selection.recent.retain(|id| id != media);
            selection.recent.push_back(media.clone());
            if selection.recent.len() > RECENT_LIMIT {
                selection.recent.pop_front();
            }
        }
        if clock.options.is_some()
            && let Some(metadata) = &clock.metadata
        {
            if selection
                .context
                .as_ref()
                .is_some_and(|context| *context != metadata.context_uri)
            {
                self.clear();
                return;
            }
            selection.context = Some(metadata.context_uri.clone());
        }
        if let Some((queue, _)) = queue {
            selection.native_next = queue
                .upcoming
                .first()
                .filter(|row| {
                    matches!(
                        row.source,
                        Some(QueueEntrySource::Autoplay | QueueEntrySource::Queue)
                    )
                })
                .map(|row| row.media_id.clone());
        }
    }

    pub(super) fn request(&mut self, now: Instant) -> Option<Request> {
        let selection = self.selection.as_mut()?;
        if selection.generation.is_none()
            || selection.ticket.is_some()
            || now < selection.fetch_after
            || !selection.candidates.is_empty()
        {
            return None;
        }
        self.serial = self.serial.wrapping_add(1).max(1);
        selection.ticket = Some(self.serial);
        // A single bounded catalog request at a time, including during API cooldown.
        selection.fetch_after = now + Duration::from_secs(10);
        Some(Request {
            ticket: self.serial,
            seed: selection.seed.clone(),
            profile: selection.profile.clone(),
            cursor: selection.cursor,
        })
    }

    pub(super) fn receive(&mut self, ticket: u64, page: Option<Page>) {
        let Some(selection) = self.selection.as_mut().filter(|s| s.ticket == Some(ticket)) else {
            return;
        };
        selection.ticket = None;
        if let Some(page) = page.filter(|page| page.ticket == ticket) {
            selection.profile = Some(page.profile);
            selection.cursor = page.cursor;
            for id in page.candidates {
                if selection.recent.contains(&id) {
                    if !selection.recycle.contains(&id) {
                        selection.recycle.push_back(id);
                    }
                } else {
                    selection.candidates.push_back(id);
                }
            }
            while selection.recycle.len() > RECENT_LIMIT {
                selection.recycle.pop_front();
            }
            if selection.candidates.is_empty()
                && page.cursor.artist == 0
                && page.cursor.offsets == [0; 8]
            {
                // A tiny catalogue can run out of new tracks. Revisit only
                // previously eligible suggestions, oldest first.
                for id in &selection.recent {
                    if selection.current.as_ref() != Some(id) && selection.recycle.contains(id) {
                        selection.candidates.push_back(id.clone());
                    }
                }
                selection.recycle.clear();
            }
        }
    }

    pub(super) fn take_candidate(
        &mut self,
        clock: &RemoteOutputClock,
        queue: &RemoteOutputQueue,
        queue_age: Duration,
        clock_age: Duration,
    ) -> Option<MediaId> {
        let selection = self.selection.as_mut()?;
        let metadata = clock.metadata.as_ref()?;
        if selection.generation != Some(clock.generation)
            || selection.current.as_ref() != clock.media_id.as_ref()
            || selection.attempted
            || clock.state != PlaybackState::Playing
            || clock.rate <= 0.0
            || !clock.rate.is_finite()
            || clock_age > super::output_clock::CONTROL_FRESHNESS
            || clock.options.as_ref()?.repeat != RepeatMode::Off
            || !clock.actions.as_ref()?.queue_add
            || !queue.autoplay_ready
            || queue_age > EMPTY_QUEUE_FRESHNESS
            || !queue.upcoming.is_empty()
        {
            return None;
        }
        // The same positive clock advances between supervisor heartbeats. Use
        // its receipt age only for tail timing, not for the published frame.
        let elapsed_ms = (clock_age.as_secs_f64() * clock.rate * 1_000.0) as u64;
        let position_ms = clock.position_ms.saturating_add(elapsed_ms);
        let remaining = metadata.duration_ms.checked_sub(position_ms)?;
        if !(1..=END_WINDOW_MS).contains(&remaining) {
            return None;
        }
        selection
            .candidates
            .retain(|id| selection.current.as_ref() != Some(id));
        let index = selection
            .candidates
            .iter()
            .position(|id| {
                !selection.recent.contains(id)
                    && !queue.previous.iter().any(|row| row.media_id == *id)
            })
            .unwrap_or(0);
        let id = selection.candidates.remove(index)?;
        selection.attempted = true;
        selection.offered = Some(id.clone());
        Some(id)
    }

    pub(super) fn is_offered(&self, id: &MediaId) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|s| s.accepted && s.offered.as_ref() == Some(id))
    }

    pub(super) fn acknowledge(&mut self, uri: &str) -> bool {
        if let Some(selection) = self.selection.as_mut()
            && !selection.accepted
            && selection
                .offered
                .as_ref()
                .is_some_and(|id| id.as_str() == uri)
        {
            selection.accepted = true;
            return true;
        }
        false
    }
}

impl Request {
    pub(super) fn ticket(&self) -> u64 {
        self.ticket
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoplay_trace::{Stage, Trace};

    fn id(kind: &str, letter: char) -> MediaId {
        MediaId::parse(format!("spotify:{kind}:{}", letter.to_string().repeat(22))).unwrap()
    }

    fn clock(letter: char, context: Option<&MediaId>) -> RemoteOutputClock {
        RemoteOutputClock {
            generation: 3,
            sequence: 5,
            device_id: "auvra".into(),
            media_id: Some(id("track", letter)),
            state: PlaybackState::Playing,
            position_ms: 19_000,
            rate: 1.0,
            active: true,
            volume: None,
            options: Some(RemoteOutputOptions {
                shuffle: false,
                repeat: RepeatMode::Off,
                enhancement: None,
            }),
            actions: Some(RemoteOutputActions {
                queue_add: true,
                ..RemoteOutputActions::default()
            }),
            metadata: Some(RemoteOutputMetadata {
                title: "Original fixture".into(),
                artists: vec!["Fixture artist".into()],
                album: None,
                duration_ms: 20_000,
                artwork: None,
                context_uri: context.map(|id| id.as_str().into()),
            }),
            control_uptime_ms: Some(123),
            queue_epoch: Some(1),
            confirmed_same_media_next: None,
        }
    }

    fn queue(clock: &RemoteOutputClock) -> RemoteOutputQueue {
        RemoteOutputQueue {
            generation: clock.generation,
            device_id: clock.device_id.clone(),
            media_id: clock.media_id.clone().unwrap(),
            epoch: clock.queue_epoch.unwrap(),
            sequence: 1,
            autoplay_ready: true,
            previous: vec![],
            upcoming: vec![],
        }
    }

    fn row(letter: char, source: QueueEntrySource) -> RemoteOutputQueueEntry {
        RemoteOutputQueueEntry {
            media_id: id("track", letter),
            source: Some(source),
            metadata: None,
        }
    }

    fn supply(radio: &mut Autoplay, now: Instant, seed: &MediaId) {
        let request = radio.request(now).unwrap();
        assert_eq!(request.seed.id(), seed);
        radio.receive(
            request.ticket,
            Some(Page {
                ticket: request.ticket,
                profile: Profile {
                    seed_artists: vec![],
                    collaborators: vec![],
                    title: None,
                    year: None,
                },
                candidates: vec![id("track", 'c'), id("track", 'd')],
                cursor: PageCursor::default(),
            }),
        );
    }

    #[test]
    fn autoplay_tail_uses_fresh_clock_age_and_cannot_offer_at_or_after_eof() {
        let now = Instant::now();
        let album = id("album", 'a');
        let mut radio = Autoplay::default();
        radio.control(
            &serde_json::from_value(serde_json::json!({
                "kind": "start_context", "uri": album.as_str(), "device_id": "auvra"
            }))
            .unwrap(),
            now,
        );
        let mut clock = clock('s', Some(&album));
        clock.position_ms = 18_475;
        let queue = queue(&clock);
        radio.observe(&clock, Some((&queue, Duration::ZERO)), now);
        supply(&mut radio, now, &album);

        assert_eq!(
            radio.take_candidate(&clock, &queue, Duration::ZERO, Duration::from_millis(124)),
            None
        );
        assert_eq!(
            radio.take_candidate(&clock, &queue, Duration::ZERO, Duration::from_millis(901)),
            None
        );
        let mut ending = clock.clone();
        ending.position_ms = 19_999;
        assert_eq!(
            radio.take_candidate(&ending, &queue, Duration::ZERO, Duration::from_millis(1)),
            None
        );
        assert_eq!(
            radio.take_candidate(&ending, &queue, Duration::ZERO, Duration::from_millis(2)),
            None
        );
        ending.rate = f64::INFINITY;
        assert_eq!(
            radio.take_candidate(&ending, &queue, Duration::ZERO, Duration::ZERO),
            None
        );

        let age = Duration::from_millis(125);
        assert_eq!(
            radio.take_candidate(&clock, &queue, Duration::ZERO, age),
            Some(id("track", 'c'))
        );
        assert_eq!(
            radio.take_candidate(&clock, &queue, Duration::ZERO, age),
            None
        );
        assert_eq!(clock.position_ms, 18_475);
    }

    #[test]
    fn autoplay_trace_requires_a_fresh_exact_head_and_acknowledgment_for_auvra() {
        let now = Instant::now();
        let previous = clock('s', None);
        let mut next = clock('c', None);
        next.sequence += 1;
        next.queue_epoch = Some(2);
        let mut trace = Trace::default();
        assert_eq!(trace.observe(Some(&previous), None, now), None);
        let mut radio = Autoplay::default();
        radio.control(
            &serde_json::from_value(serde_json::json!({
                "kind": "start_context", "uri": id("album", 'a').as_str(), "device_id": "auvra"
            }))
            .unwrap(),
            now,
        );
        radio.selection.as_mut().unwrap().offered = next.media_id.clone();
        let mut head = queue(&previous);
        head.upcoming = vec![row('c', QueueEntrySource::Autoplay)];
        let origin = |head: &RemoteOutputQueue, age, radio: &Autoplay| {
            trace.origin(Some(&next), Some((head, age)), radio)
        };
        assert_eq!(
            origin(&head, Duration::ZERO, &radio),
            Some(Stage::NextSpotify)
        );
        assert_eq!(origin(&head, Duration::from_millis(1_201), &radio), None);
        head.upcoming[0].source = Some(QueueEntrySource::Queue);
        assert_eq!(origin(&head, Duration::ZERO, &radio), None);
        assert!(!radio.acknowledge(id("track", 'x').as_str()));
        assert!(radio.acknowledge(id("track", 'c').as_str()));
        assert!(!radio.acknowledge(id("track", 'c').as_str()));
        assert_eq!(
            origin(&head, Duration::ZERO, &radio),
            Some(Stage::NextAuvra)
        );
        head.upcoming[0].source = Some(QueueEntrySource::Autoplay);
        assert_eq!(origin(&head, Duration::ZERO, &radio), None);
        head.upcoming[0].source = Some(QueueEntrySource::Queue);
        head.upcoming.push(row('c', QueueEntrySource::Autoplay));
        assert_eq!(origin(&head, Duration::ZERO, &radio), None);
        head.upcoming.pop();
        head.generation += 1;
        assert_eq!(origin(&head, Duration::ZERO, &radio), None);
        head.generation -= 1;
        head.epoch += 1;
        assert_eq!(origin(&head, Duration::ZERO, &radio), None);
    }

    #[test]
    fn autoplay_trace_reports_once_only_after_the_new_track_is_playing() {
        let now = Instant::now();
        let previous = clock('s', None);
        let mut next = clock('c', None);
        next.sequence += 1;
        next.queue_epoch = Some(2);
        next.state = PlaybackState::Loading;
        next.rate = 0.0;
        let mut trace = Trace::default();
        assert_eq!(trace.observe(Some(&previous), None, now), None);
        assert_eq!(
            trace.observe(Some(&next), Some(Stage::NextSpotify), now),
            None
        );
        next.sequence += 1;
        next.state = PlaybackState::Playing;
        next.rate = 1.0;
        assert_eq!(
            trace.observe(Some(&next), None, now),
            Some(Stage::NextSpotify)
        );
        assert_eq!(trace.observe(Some(&next), None, now), None);
        next.sequence += 1;
        next.media_id = Some(id("track", 'x'));
        assert_eq!(
            trace.observe(Some(&next), None, now),
            Some(Stage::NextUnknown)
        );
        next.generation += 1;
        next.media_id = Some(id("track", 'y'));
        assert_eq!(
            trace.observe(Some(&next), Some(Stage::NextAuvra), now),
            None
        );
    }

    #[test]
    fn standalone_keeps_its_seed_after_each_fallback_and_waits_for_fresh_empty_queue() {
        let now = Instant::now();
        let mut radio = Autoplay::default();
        let mut selected = clock('s', Some(&id("album", 'a')));
        let item = mapping::track(&serde_json::json!({
            "id": "s".repeat(22), "name": "Fixture", "type": "track"
        }))
        .unwrap();
        radio.control(
            &RemoteControl::StartOnDevice {
                item,
                device_id: selected.device_id.clone(),
                operation_id: Some(5),
            },
            now,
        );
        let mut first_queue = queue(&selected);
        radio.observe(&selected, Some((&first_queue, Duration::ZERO)), now);
        supply(&mut radio, now, &id("track", 's'));
        first_queue
            .upcoming
            .push(row('n', QueueEntrySource::Autoplay));
        assert!(
            radio
                .take_candidate(&selected, &first_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );
        first_queue.upcoming.clear();
        first_queue.autoplay_ready = false;
        assert!(
            radio
                .take_candidate(&selected, &first_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );
        first_queue.autoplay_ready = true;
        assert!(
            radio
                .take_candidate(
                    &selected,
                    &first_queue,
                    Duration::from_secs(2),
                    Duration::ZERO
                )
                .is_none()
        );
        selected.position_ms = 1_000;
        assert!(
            radio
                .take_candidate(&selected, &first_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );
        selected.position_ms = 19_000;
        let chosen = radio
            .take_candidate(&selected, &first_queue, Duration::ZERO, Duration::ZERO)
            .unwrap();
        assert_eq!(chosen, id("track", 'c'));
        assert!(
            radio
                .take_candidate(&selected, &first_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );
        radio.acknowledge(chosen.as_str());
        assert!(radio.is_offered(&chosen));

        let continuation = clock('c', None);
        let next = queue(&continuation);
        radio.observe(&continuation, Some((&next, Duration::ZERO)), now);
        assert_eq!(
            radio.take_candidate(&continuation, &next, Duration::ZERO, Duration::ZERO),
            Some(id("track", 'd'))
        );
        let third = clock('d', None);
        let third_queue = queue(&third);
        radio.observe(&third, Some((&third_queue, Duration::ZERO)), now);
        supply(&mut radio, now + Duration::from_secs(11), &id("track", 's'));
        assert_eq!(
            radio.take_candidate(&third, &third_queue, Duration::ZERO, Duration::ZERO),
            Some(id("track", 'c'))
        );
    }

    #[test]
    fn album_waits_for_its_last_entry_and_preserves_native_repeat_pause_and_device_authority() {
        let now = Instant::now();
        let album = id("album", 'a');
        let mut radio = Autoplay::default();
        // Selection tracks album identity even if a different recording is the last item.
        radio.control(
            &serde_json::from_value(serde_json::json!({
                "kind": "start_context", "uri": album.as_str(), "device_id": "auvra"
            }))
            .unwrap(),
            now,
        );
        let first = clock('f', Some(&album));
        let mut first_queue = queue(&first);
        first_queue
            .upcoming
            .push(row('l', QueueEntrySource::Context));
        radio.observe(&first, Some((&first_queue, Duration::ZERO)), now);
        supply(&mut radio, now, &album);
        assert!(
            radio
                .take_candidate(&first, &first_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );

        let mut last = clock('l', Some(&album));
        let last_queue = queue(&last);
        radio.observe(&last, Some((&last_queue, Duration::ZERO)), now);
        for mode in [RepeatMode::All, RepeatMode::One] {
            last.options.as_mut().unwrap().repeat = mode;
            assert!(
                radio
                    .take_candidate(&last, &last_queue, Duration::ZERO, Duration::ZERO)
                    .is_none()
            );
        }
        last.options.as_mut().unwrap().repeat = RepeatMode::Off;
        last.state = PlaybackState::Paused;
        assert!(
            radio
                .take_candidate(&last, &last_queue, Duration::ZERO, Duration::ZERO)
                .is_none()
        );
        last.state = PlaybackState::Playing;
        last.device_id = "another-device".into();
        radio.observe(&last, None, now);
        assert!(radio.selection.is_none());
    }
}
