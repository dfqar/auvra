use super::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
};

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    body: Value,
    authorized: bool,
    content_length: Option<usize>,
}
struct Server {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    mode: Arc<AtomicU16>,
    delay: Arc<AtomicU64>,
    retry_after: Arc<Mutex<Option<String>>>,
    rate_reply: Arc<Mutex<Option<String>>>,
    hold_rate_body: Arc<AtomicBool>,
    hold_playback: Arc<AtomicBool>,
    empty_device_reads: Arc<AtomicU64>,
    hold_previous_response: Arc<AtomicBool>,
    hold_volume_response: Arc<AtomicBool>,
    hold_next_response: Arc<AtomicBool>,
    hold_play_response: Arc<AtomicBool>,
    fail_playback_reads: Arc<AtomicBool>,
    hold_positioned_playback: Arc<AtomicBool>,
    hold_resume_probe: Arc<AtomicBool>,
    stateful_spoken_library: Arc<AtomicBool>,
    playlist_version: Arc<AtomicU64>,
    change_on_items: Arc<AtomicBool>,
    change_on_move: Arc<AtomicBool>,
    applied_moves: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
fn track(id: char) -> Value {
    json!({"id":id.to_string().repeat(22),"type":"track","name":"Original fixture tone","duration_ms":8000,"artists":[{"id":"r".repeat(22),"name":"Auvra test author"}],"album":{"id":"m".repeat(22),"name":"Original test album","images":[]},"is_playable":true,"is_local":false})
}
#[test]
fn playlist_edit_metadata_preserves_owner_and_collaboration_without_inferring_from_visibility() {
    let mut value = json!({"id":"p".repeat(22),"name":"Original playlist","public":true,"owner":{"id":"original-owner"},"collaborative":false});
    let playlist = mapping::collection(&value).unwrap();
    assert_eq!(playlist.owner_id.as_deref(), Some("original-owner"));
    assert_eq!(playlist.collaborative, Some(false));
    value["collaborative"] = json!(true);
    value["public"] = json!(false);
    assert_eq!(
        mapping::collection(&value).unwrap().collaborative,
        Some(true)
    );
    assert!(
        mapping::collection_as(&value, RemoteEntityKind::Album)
            .unwrap()
            .owner_id
            .is_none()
    );
    value["owner"] = Value::Null;
    value["collaborative"] = Value::Null;
    let unknown = mapping::collection(&value).unwrap();
    assert!(unknown.owner_id.is_none() && unknown.collaborative.is_none());
}
fn frame() -> Value {
    json!({"is_playing":true,"progress_ms":1234,"item":track('1'),"shuffle_state":false,"repeat_state":"off","device":{"id":"fixture-device","name":"Test Connect speaker","type":"Computer","volume_percent":30,"supports_volume":true,"is_active":true,"is_restricted":false},"actions":{"disallows":{}}})
}
fn output_base(id: u64) -> RemoteState {
    let mut state = disconnected(false);
    state.connection = ProviderConnectionState::Connected;
    state.observation_id = id;
    state.playback = Some(mapping::playback(&frame()));
    state
}
fn output_clock(sequence: u64) -> RemoteOutputClock {
    RemoteOutputClock {
        generation: 1,
        sequence,
        device_id: "fixture-device".into(),
        media_id: Some(MediaId::parse(format!("spotify:track:{}", "1".repeat(22))).unwrap()),
        state: PlaybackState::Playing,
        position_ms: 3000,
        rate: 1.5,
        active: true,
        volume: None,
        options: None,
        actions: None,
        metadata: None,
        control_uptime_ms: None,
        queue_epoch: None,
        confirmed_same_media_next: None,
    }
}
mod chapter_catalog;
mod context_item_start;
mod direct_owned_start;
mod external_queue;
mod owned_autoplay;
mod owned_metadata;
mod owned_queue;
mod owned_stale_destination;
mod owned_transport;
mod resume_context;
mod spoken_library;
mod spoken_start;
mod targeted_pause;
#[test]
fn owned_clock_preserves_metadata_and_applies_actual_state_and_speed_without_pcm_capabilities() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    let first = p.take(now).unwrap();
    p.observe(Some(output_clock(1)), now + Duration::from_secs(1))
        .unwrap();
    let actual = p.take(now + Duration::from_secs(1)).unwrap();
    let frame = actual.playback.as_ref().unwrap();
    assert_eq!(frame.position_ms, 3000);
    assert_eq!(frame.position_rate, 1.5);
    assert_eq!(
        frame.current.as_ref().unwrap().title,
        first.playback.unwrap().current.unwrap().title
    );
    assert!(
        !frame.capabilities.pcm_analysis
            && !frame.capabilities.system_media_session
            && !frame.capabilities.equalizer
    );
    let mut paused = output_clock(2);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    paused.position_ms = 4500;
    p.observe(Some(paused), now + Duration::from_secs(2))
        .unwrap();
    let actual = p.take(now + Duration::from_secs(2)).unwrap();
    assert_eq!(
        actual.playback.as_ref().unwrap().state,
        PlaybackState::Paused
    );
    assert_eq!(actual.playback.as_ref().unwrap().position_ms, 4500);
    assert_ne!(actual.observation_id, first.observation_id);
}
#[test]
fn owned_clock_never_attaches_to_a_foreign_device_or_recording_and_refresh_is_coalesced() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    let initial = p.take(now).unwrap().observation_id;
    let mut foreign = output_clock(1);
    foreign.device_id = "phone".into();
    assert!(p.observe(Some(foreign.clone()), now).unwrap());
    let rejected = p.take(now).unwrap();
    assert_eq!(rejected.observation_id, initial);
    assert_eq!(rejected.playback.unwrap().position_ms, 1234);
    foreign.sequence = 2;
    assert!(!p.observe(Some(foreign), now).unwrap());
    p.take(now);
    let mut other = output_clock(3);
    other.media_id = Some(MediaId::parse(format!("spotify:track:{}", "2".repeat(22))).unwrap());
    assert!(p.observe(Some(other), now).unwrap());
    assert_eq!(
        p.take(now).unwrap().playback.unwrap().current.unwrap().id,
        output_clock(1).media_id.unwrap()
    );
}
#[test]
fn owned_clock_bridges_only_a_positive_matching_204_and_expires_to_the_http_projection() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    p.observe(Some(output_clock(1)), now).unwrap();
    p.take(now);
    let mut empty = output_base(2);
    empty.playback = None;
    p.ingest(empty, now + Duration::from_secs(1));
    assert_eq!(
        p.take(now + Duration::from_secs(1))
            .unwrap()
            .playback
            .unwrap()
            .position_ms,
        4500
    );
    assert!(
        p.take(now + Duration::from_secs(7))
            .unwrap()
            .playback
            .is_none()
    );
    assert!(p.take(now + Duration::from_secs(8)).is_none());
}
#[test]
fn owned_device_loss_refreshes_once_per_transition_instead_of_waiting_for_background_poll() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    assert!(!p.observe(Some(output_clock(1)), now).unwrap());
    p.take(now);
    let mut inactive = output_clock(2);
    inactive.active = false;
    assert!(p.observe(Some(inactive.clone()), now).unwrap());
    p.take(now);
    inactive.sequence = 3;
    assert!(!p.observe(Some(inactive), now).unwrap());
    p.take(now);
    assert!(!p.observe(Some(output_clock(4)), now).unwrap());
    p.take(now);
    let mut again = output_clock(5);
    again.active = false;
    assert!(p.observe(Some(again), now).unwrap());
}
#[test]
fn owned_clock_sequence_and_generation_guards_do_not_extend_a_stale_lease() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    let mut clock = output_clock(4);
    clock.generation = 2;
    p.observe(Some(clock.clone()), now).unwrap();
    let actual = p.take(now).unwrap();
    p.observe(Some(output_clock(100)), now + Duration::from_secs(4))
        .unwrap();
    p.observe(Some(clock), now + Duration::from_secs(5))
        .unwrap();
    assert!(p.take(now + Duration::from_secs(5)).is_none());
    let expired = p.take(now + Duration::from_secs(7)).unwrap();
    assert_eq!(expired.observation_id, actual.observation_id);
    assert_eq!(expired.playback.unwrap().position_ms, 1234);
}
#[test]
fn owned_clock_is_revoked_by_credentials_logout_and_inactive_output_without_replay() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    p.observe(Some(output_clock(1)), now).unwrap();
    p.take(now);
    let mut inactive = output_clock(2);
    inactive.active = false;
    p.observe(Some(inactive), now).unwrap();
    assert_eq!(p.take(now).unwrap().playback.unwrap().position_ms, 1234);
    p.reset();
    p.ingest(output_base(1), now);
    p.observe(Some(output_clock(1)), now).unwrap();
    assert_eq!(p.take(now).unwrap().playback.unwrap().position_ms, 1234);
    p.observe(Some(output_clock(3)), now).unwrap();
    p.take(now);
    p.ingest(disconnected(true), now);
    assert!(p.take(now).unwrap().playback.is_none());
    p.ingest(output_base(2), now);
    assert_eq!(p.take(now).unwrap().playback.unwrap().position_ms, 1234);
}
#[test]
fn owned_clock_rejects_invalid_numbers_identity_and_state_before_mutating_the_projection() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    for bad in 0..7 {
        let mut clock = output_clock(1);
        match bad {
            0 => clock.rate = f64::NAN,
            1 => clock.rate = 4.1,
            2 => clock.position_ms = 86_400_001,
            3 => clock.sequence = 0,
            4 => clock.generation = 0,
            5 => clock.state = PlaybackState::Error,
            _ => clock.media_id = None,
        }
        assert!(p.observe(Some(clock), now).is_err());
    }
    p.ingest(output_base(1), now);
    assert_eq!(p.take(now).unwrap().playback.unwrap().position_ms, 1234);
}
#[test]
fn owned_clock_bypasses_blocked_http_without_sending_a_playback_command() {
    let server = Server::new();
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    connect(&mut provider);
    server.delay.store(800, Ordering::SeqCst);
    provider.submit(RemoteAction::Refresh).unwrap();
    thread::sleep(Duration::from_millis(40));
    let start = Instant::now();
    let mut paused = output_clock(1);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(paused),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused && p.position_ms == 3000)),
    );
    assert!(start.elapsed() < Duration::from_millis(250));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
    server.delay.store(0, Ordering::SeqCst);
}
fn output_actions() -> RemoteOutputActions {
    RemoteOutputActions {
        play: true,
        seek: true,
        shuffle: true,
        repeat: true,
        queue_add: true,
        seek_forward_ms: Some(15_000),
        seek_backward_ms: Some(30_000),
        ..Default::default()
    }
}
#[test]
fn owned_actions_restore_play_after_pause_and_project_options_volume_and_preferred_skips() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    let mut raw = frame();
    raw["actions"]["disallows"]["resuming"] = json!(true);
    let mut state = output_base(1);
    state.playback = Some(mapping::playback(&raw));
    state.devices = vec![state.playback.as_ref().unwrap().device.clone().unwrap()];
    assert!(!state.playback.as_ref().unwrap().capabilities.play);
    p.ingest(state, now);
    p.take(now);
    let mut clock = output_clock(1);
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.actions = Some(output_actions());
    clock.volume = Some(0.42);
    clock.options = Some(RemoteOutputOptions {
        shuffle: true,
        repeat: RepeatMode::One,
        enhancement: Some(PlaybackEnhancement::Recommendations),
    });
    p.observe(Some(clock), now).unwrap();
    let state = p.take(now).unwrap();
    let frame = state.playback.unwrap();
    assert!(frame.capabilities.play && !frame.capabilities.pause && frame.capabilities.seek);
    assert!(
        frame.transport.queue_add
            && frame.transport.shuffle
            && frame.transport.repeat
            && !frame.transport.next
    );
    assert_eq!(
        (
            frame.transport.seek_forward_ms,
            frame.transport.seek_backward_ms
        ),
        (Some(15_000), Some(30_000))
    );
    assert!(frame.shuffle);
    assert_eq!(frame.repeat, RepeatMode::One);
    assert_eq!(
        frame.enhancement,
        Some(PlaybackEnhancement::Recommendations)
    );
    assert_eq!(frame.volume, 0.42);
    assert_eq!(state.devices[0].volume, Some(0.42));
    assert!(
        !frame.capabilities.pcm_analysis
            && !frame.capabilities.system_media_session
            && !frame.capabilities.equalizer
            && !frame.transport.queue_edit
            && !frame.transport.stop
    );
}
#[test]
fn empty_owned_actions_are_distinct_from_an_omitted_advertisement_and_expiry_returns_http() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    p.observe(Some(output_clock(1)), now).unwrap();
    assert!(p.take(now).unwrap().playback.unwrap().capabilities.pause);
    let mut empty = output_clock(2);
    empty.actions = Some(RemoteOutputActions::default());
    p.observe(Some(empty), now).unwrap();
    let actual = p.take(now).unwrap().playback.unwrap();
    assert!(
        !actual.capabilities.play
            && !actual.capabilities.pause
            && !actual.capabilities.seek
            && !actual.transport.next
            && !actual.transport.queue_add
    );
    let restored = p
        .take(now + Duration::from_secs(7))
        .unwrap()
        .playback
        .unwrap();
    assert!(restored.capabilities.pause && restored.transport.queue_add);
    assert_eq!(restored.transport.seek_forward_ms, None);
}
#[test]
fn mismatched_owned_extras_cannot_change_an_external_output_or_leak_after_clear() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    let mut other = output_clock(1);
    other.device_id = "phone".into();
    other.volume = Some(0.9);
    other.actions = Some(output_actions());
    other.options = Some(RemoteOutputOptions {
        shuffle: true,
        repeat: RepeatMode::One,
        enhancement: Some(PlaybackEnhancement::Recommendations),
    });
    p.observe(Some(other.clone()), now).unwrap();
    let unchanged = p.take(now).unwrap().playback.unwrap();
    assert_eq!(unchanged.volume, 0.3);
    assert!(!unchanged.shuffle);
    assert!(unchanged.enhancement.is_none());
    other.device_id = "fixture-device".into();
    other.sequence = 2;
    p.observe(Some(other), now).unwrap();
    p.take(now);
    p.observe(None, now).unwrap();
    let cleared = p.take(now).unwrap().playback.unwrap();
    assert_eq!(cleared.volume, 0.3);
    assert_eq!(cleared.repeat, RepeatMode::Off);
    assert!(cleared.enhancement.is_none());
}
#[test]
fn owned_action_payload_rejects_invalid_volume_and_skip_bounds_without_publishing() {
    let now = Instant::now();
    let mut p = output_clock::OutputProjection::default();
    p.ingest(output_base(1), now);
    p.take(now);
    for volume in [f32::NAN, -0.1, 1.1] {
        let mut clock = output_clock(1);
        clock.volume = Some(volume);
        assert!(p.observe(Some(clock), now).is_err());
    }
    for step in [0, 300_001] {
        let mut clock = output_clock(1);
        let mut actions = output_actions();
        actions.seek_forward_ms = Some(step);
        clock.actions = Some(actions);
        assert!(p.observe(Some(clock), now).is_err());
    }
    assert!(p.take(now).is_none());
}
#[test]
fn chapter_queue_add_is_rejected_before_http_but_episode_uri_is_accepted() {
    let server = Server::new();
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    let mut chapter = track('c');
    chapter["type"] = json!("chapter");
    let chapter = mapping::track(&chapter).unwrap();
    assert!(mapping::track_uri(&chapter).is_ok());
    assert!(
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Enqueue { item: chapter }
            })
            .is_err()
    );
    assert!(server.requests.lock().unwrap().is_empty());
    let mut episode = track('e');
    episode["type"] = json!("episode");
    assert!(
        mapping::queue_uri(&mapping::track(&episode).unwrap())
            .unwrap()
            .starts_with("spotify:episode:")
    );
}
impl Server {
    fn new() -> Self {
        Self::with_profile(json!({
            "id":"original-test-user",
            "account_id":"original-stable-account",
            "display_name":"Original fixture listener"
        }))
    }
    fn with_profile(profile: Value) -> Self {
        Self::with_catalog_data(
            profile,
            json!({"items":[{"track":track('4')}],"next":"https://api.spotify.com/next","cursors":{"before":"1740000000000"}}),
        )
    }
    fn with_catalog_data(profile: Value, history: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let mode = Arc::new(AtomicU16::new(200));
        let delay = Arc::new(AtomicU64::new(0));
        let retry_after = Arc::new(Mutex::new(Some("1".to_owned())));
        let rate_reply = Arc::new(Mutex::new(None::<String>));
        let hold_rate_body = Arc::new(AtomicBool::new(false));
        let hold_playback = Arc::new(AtomicBool::new(false));
        let empty_device_reads = Arc::new(AtomicU64::new(0));
        let hold_previous_response = Arc::new(AtomicBool::new(false));
        let hold_volume_response = Arc::new(AtomicBool::new(false));
        let hold_next_response = Arc::new(AtomicBool::new(false));
        let hold_play_response = Arc::new(AtomicBool::new(false));
        let fail_playback_reads = Arc::new(AtomicBool::new(false));
        let hold_positioned_playback = Arc::new(AtomicBool::new(false));
        let hold_resume_probe = Arc::new(AtomicBool::new(false));
        let stateful_spoken_library = Arc::new(AtomicBool::new(false));
        let playlist_version = Arc::new(AtomicU64::new(1));
        let change_on_items = Arc::new(AtomicBool::new(false));
        let change_on_move = Arc::new(AtomicBool::new(false));
        let applied_moves = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let records = requests.clone();
        let status = mode.clone();
        let wait = delay.clone();
        let retry_header = retry_after.clone();
        let rate_response = rate_reply.clone();
        let hold_body = hold_rate_body.clone();
        let hold_player = hold_playback.clone();
        let empty_devices = empty_device_reads.clone();
        let hold_previous = hold_previous_response.clone();
        let hold_volume = hold_volume_response.clone();
        let hold_next = hold_next_response.clone();
        let hold_play = hold_play_response.clone();
        let fail_player = fail_playback_reads.clone();
        let hold_positioned_player = hold_positioned_playback.clone();
        let hold_probe = hold_resume_probe.clone();
        let track_membership = stateful_spoken_library.clone();
        let stopping = stop.clone();
        let version = playlist_version.clone();
        let change_items = change_on_items.clone();
        let change_move = change_on_move.clone();
        let moves = applied_moves.clone();
        let worker = thread::spawn(move || {
            let mut playback = frame();
            let mut delayed_start: Option<(char, u8)> = None;
            let mut saved_spoken = HashSet::new();
            let mut rejected_direct_play = false;
            let mut positioned_start = false;
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(3));
                        continue;
                    }
                };
                // macOS inherits O_NONBLOCK from the listener; a request may arrive after accept.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut data = vec![];
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap_or(0);
                    if count == 0 {
                        break;
                    }
                    data.extend_from_slice(&buffer[..count]);
                    if let Some(header) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..header]);
                        let length = headers
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= header + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&data);
                let mut first = text.lines().next().unwrap_or_default().split_whitespace();
                let method = first.next().unwrap_or_default().to_owned();
                let target = first.next().unwrap_or_default().to_owned();
                if target.is_empty() {
                    continue;
                }
                let authorized = text
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("authorization: Bearer fixture-bearer"));
                let body = text
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str(body).ok())
                    .unwrap_or(Value::Null);
                let content_length = text.lines().find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                });
                records.lock().unwrap().push(Request {
                    method: method.clone(),
                    target: target.clone(),
                    body: body.clone(),
                    authorized,
                    content_length,
                });
                thread::sleep(Duration::from_millis(wait.load(Ordering::SeqCst)));
                let url = reqwest::Url::parse(&format!("http://test{target}")).unwrap();
                let query = |name: &str| {
                    url.query_pairs()
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default()
                };
                let previous_playback = playback.clone();
                let mut code = status.load(Ordering::SeqCst);
                let use_membership = track_membership.load(Ordering::SeqCst);
                let mut value = match (method.as_str(), url.path()) {
                    ("GET", path) if query("limit") == "50" && resume_context::probe_path(path) => {
                        while hold_probe.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst)
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        resume_context::probe_response(path, &query("offset"), &mut code)
                    }
                    ("GET", "/v1/me/player") => {
                        if fail_player.load(Ordering::SeqCst) {
                            continue;
                        }
                        while (hold_player.load(Ordering::SeqCst)
                            || positioned_start && hold_positioned_player.load(Ordering::SeqCst))
                            && !stopping.load(Ordering::SeqCst)
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        if let Some((id, remaining)) = delayed_start.as_mut() {
                            if *remaining == 0 {
                                playback["item"] = track(*id);
                                playback["is_playing"] = json!(true);
                                delayed_start = None;
                            } else {
                                *remaining -= 1;
                            }
                        }
                        playback.clone()
                    }
                    ("GET", "/v1/me/player/queue") => {
                        json!({"currently_playing":playback["item"],"queue":[track('2'),track('3')]})
                    }
                    ("GET", "/v1/me/player/devices") => {
                        if empty_devices.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
                            json!({"devices":[]})
                        } else {
                            json!({"devices":[playback["device"].clone()]})
                        }
                    }
                    ("GET", "/v1/search") => {
                        let kind = query("type");
                        if kind == "track" && query("q") == "artist:\"Auvra test author\"" {
                            let mut result = track('b');
                            result["name"] = json!("Related original fixture");
                            let mut unrelated = track('c');
                            unrelated["artists"][0]["id"] = json!("v".repeat(22));
                            let mut same_recording = track('x');
                            same_recording["name"] = json!("Original fixture tone - remastered");
                            json!({"tracks":{"total":4,"items":[track('1'), unrelated, same_recording, result]}})
                        } else if kind == "track" && query("q").starts_with("artist:") {
                            let mut other = track('9');
                            other["artists"][0]["id"] = json!("w".repeat(22));
                            let mut unavailable = track('8');
                            unavailable["is_playable"] = json!(false);
                            json!({"tracks":{"total":5,"items":[other,track('6'),track('6'),unavailable,track('4')]}})
                        } else if kind == "track,album,artist,playlist,show,episode,audiobook" {
                            let point = json!({"resume_position_ms":23000,"fully_played":false});
                            let mut episode =
                                spoken_progress_fixture("episode", 'e', point.clone());
                            episode.as_object_mut().unwrap().remove("type");
                            let mut music = track('4');
                            music["resume_point"] = point.clone();
                            json!({
                                "tracks":{"total":21,"items":[music,Value::Null,spoken_progress_fixture("chapter", 'e', point)]},
                                "albums":{"total":32,"items":[collection_fixture("album")]},
                                "artists":{"total":17,"items":[collection_fixture("artist"),collection_fixture("album")]},
                                "playlists":{"total":47,"items":[collection_fixture("playlist")]},
                                "shows":{"total":15,"items":[collection_fixture("show")]},
                                "episodes":{"total":24,"items":[episode]},
                                "audiobooks":{"total":11,"items":[collection_fixture("audiobook")]}
                            })
                        } else if kind == "track" {
                            json!({"tracks":{"total":21,"items":[track('4')]}})
                        } else {
                            let key = format!("{kind}s");
                            let item = if kind == "episode" {
                                spoken_fixture("episode")
                            } else {
                                collection_fixture(&kind)
                            };
                            json!({key:{"total":1,"items":[item]}})
                        }
                    }
                    ("GET", "/v1/me/library/contains") if use_membership => json!(
                        query("uris")
                            .split(',')
                            .map(|uri| saved_spoken.contains(uri))
                            .collect::<Vec<_>>()
                    ),
                    ("PUT" | "DELETE", "/v1/me/library") if use_membership => {
                        let uris = query("uris");
                        for uri in uris.split(',') {
                            if method == "PUT" {
                                saved_spoken.insert(uri.to_owned());
                            } else {
                                saved_spoken.remove(uri);
                            }
                        }
                        Value::Null
                    }
                    ("GET", path @ ("/v1/me/shows" | "/v1/me/episodes" | "/v1/me/audiobooks"))
                        if use_membership =>
                    {
                        spoken_library::saved_page(path, &saved_spoken)
                    }
                    ("GET", "/v1/me/library/contains") => json!([true]),
                    ("GET", "/v1/me/albums") => {
                        json!({"total":1,"items":[{"album":collection_fixture("album")}]})
                    }
                    ("GET", "/v1/me/shows") => {
                        json!({"total":1,"items":[{"show":collection_fixture("show")}]})
                    }
                    ("GET", "/v1/me/audiobooks") => {
                        json!({"total":1,"items":[collection_fixture("audiobook")]})
                    }
                    ("GET", "/v1/me/episodes") => {
                        json!({"total":1,"items":[{"episode":spoken_progress_fixture("episode", 'e', json!({"resume_position_ms":23000,"fully_played":false}))}]})
                    }
                    ("GET", "/v1/me/top/tracks") => json!({"total":1,"items":[track('4')]}),
                    ("GET", "/v1/me/top/artists") => {
                        json!({"total":1,"items":[collection_fixture("artist")]})
                    }
                    ("GET", "/v1/me/following") => {
                        json!({"artists":{"total":2,"items":[collection_fixture("artist")],"next":"https://api.spotify.com/next","cursors":{"after":"b".repeat(22)}}})
                    }
                    ("GET", "/v1/me/player/recently-played") => history.clone(),
                    ("GET", "/v1/me") => profile.clone(),
                    ("POST", "/v1/me/playlists") => {
                        if code == 200 {
                            code = 201;
                        }
                        let mut playlist = collection_fixture("playlist");
                        playlist["public"] = body["public"].clone();
                        playlist["collaborative"] =
                            json!(body["collaborative"].as_bool().unwrap_or(false));
                        playlist
                    }
                    ("PUT", path)
                        if path.starts_with("/v1/playlists/") && path.split('/').count() == 4 =>
                    {
                        Value::Null
                    }
                    ("GET", path)
                        if path.starts_with("/v1/albums/") && path.ends_with("/tracks") =>
                    {
                        let offset = query("offset").parse::<u32>().unwrap_or_default();
                        json!({"offset":offset,"total":1,"items":[track('4')]})
                    }
                    ("GET", path)
                        if path.starts_with("/v1/artists/") && path.ends_with("/albums") =>
                    {
                        let offset = query("offset").parse::<u32>().unwrap_or_default();
                        let mut album = collection_fixture("album");
                        album["album_type"] = json!("album");
                        album["release_date"] = json!("2024-01-01");
                        json!({"offset":offset,"total":1,"items":[album]})
                    }
                    ("GET", path)
                        if path.starts_with("/v1/artists/") && path.ends_with("/top-tracks") =>
                    {
                        json!({"tracks":[track('4'),track('6')]})
                    }
                    ("GET", path)
                        if path.starts_with("/v1/shows/") && path.ends_with("/episodes") =>
                    {
                        if path == format!("/v1/shows/{}/episodes", "p".repeat(22)) {
                            let mut rows: Vec<_> = [
                                ('e', json!({"resume_position_ms":7000})),
                                (
                                    'e',
                                    json!({"resume_position_ms":23000,"fully_played":false}),
                                ),
                                (
                                    'e',
                                    json!({"resume_position_ms":84000,"fully_played":false}),
                                ),
                                ('b', json!({"resume_position_ms":-1,"fully_played":false})),
                                (
                                    'c',
                                    json!({"resume_position_ms":120001,"fully_played":true}),
                                ),
                                (
                                    'd',
                                    json!({"resume_position_ms":"1000","fully_played":false}),
                                ),
                                (
                                    'f',
                                    json!({"resume_position_ms":120000,"fully_played":true}),
                                ),
                                (
                                    'g',
                                    json!({"resume_position_ms":120000,"fully_played":false}),
                                ),
                                ('h', json!({"resume_position_ms":0,"fully_played":false})),
                            ]
                            .into_iter()
                            .map(|(id, point)| spoken_progress_fixture("episode", id, point))
                            .collect();
                            rows.push(spoken_progress_fixture(
                                "chapter",
                                'i',
                                json!({"resume_position_ms":23000,"fully_played":false}),
                            ));
                            json!({"total":rows.len(),"items":rows})
                        } else {
                            json!({"total":1,"items":[spoken_fixture("episode")]})
                        }
                    }
                    ("GET", path)
                        if path.starts_with("/v1/audiobooks/") && path.ends_with("/chapters") =>
                    {
                        chapter_catalog::chapter_page(path).unwrap_or_else(|| {
                            json!({"total":1,"items":[spoken_progress_fixture("chapter", 'e', json!({"resume_position_ms":41000,"fully_played":false}))]})
                        })
                    }
                    ("GET", path) if path.starts_with("/v1/episodes/") => spoken_progress_fixture(
                        "episode",
                        'e',
                        json!({"resume_position_ms":23000,"fully_played":false}),
                    ),
                    ("GET", path) if path.starts_with("/v1/chapters/") => {
                        chapter_catalog::chapter_detail(path).unwrap_or_else(|| {
                            spoken_progress_fixture("chapter", 'e', json!({"resume_position_ms":120000,"fully_played":true}))
                        })
                    }
                    ("GET", path) if path == format!("/v1/tracks/{}", "1".repeat(22)) => track('1'),
                    ("GET", path) if path.starts_with("/v1/tracks/") => track('4'),
                    ("GET", path) if path.starts_with("/v1/albums/") => collection_fixture("album"),
                    ("GET", path) if path.starts_with("/v1/artists/") => {
                        let mut artist = collection_fixture("artist");
                        artist["id"] = json!(path.rsplit('/').next().unwrap());
                        artist
                    }
                    ("GET", path) if path.starts_with("/v1/shows/") => collection_fixture("show"),
                    ("GET", path) if path.starts_with("/v1/audiobooks/") => {
                        collection_fixture("audiobook")
                    }
                    ("GET", "/v1/me/tracks") => json!({"total":1,"items":[{"track":track('5')}]}),
                    ("GET", "/v1/me/playlists") => {
                        json!({"total":1,"items":[{"id":"a".repeat(22),"name":"Original fixture playlist","images":[]}]})
                    }
                    ("GET", path)
                        if path.starts_with("/v1/playlists/") && path.ends_with("/images") =>
                    {
                        json!([{"url":"https://image-cdn-fa.spotifycdn.com/image/original-cover-fixture"}])
                    }
                    ("GET", path)
                        if path.starts_with("/v1/playlists/") && !path.ends_with("/items") =>
                    {
                        let mut playlist = collection_fixture("playlist");
                        playlist["snapshot_id"] = if version.load(Ordering::SeqCst) == 0 {
                            Value::Null
                        } else {
                            json!(format!(
                                "original-version-{}",
                                version.load(Ordering::SeqCst)
                            ))
                        };
                        playlist
                    }
                    ("GET", path) if path.ends_with("/items") => {
                        if change_items.swap(false, Ordering::SeqCst) {
                            version.fetch_add(1, Ordering::SeqCst);
                        }
                        let offset = query("offset").parse::<u32>().unwrap_or_default();
                        if query("limit") == "1" {
                            let mut row = track('6');
                            row["uri"] = json!(format!("spotify:track:{}", "6".repeat(22)));
                            let items = if offset == 10 || offset == 12 {
                                json!([{"item":row}])
                            } else {
                                json!([])
                            };
                            json!({"offset":offset,"total":13,"items":items})
                        } else if path.contains(&"s".repeat(22)) {
                            json!({"offset":offset,"total":13,"items":[{"item":track('6')},{"item":null},{"item":track('6')}]})
                        } else {
                            json!({"offset":offset,"total":1,"items":[{"item":track('6')}]})
                        }
                    }
                    ("PUT", path)
                        if path.ends_with("/items") && body.get("range_start").is_some() =>
                    {
                        if change_move.swap(false, Ordering::SeqCst) {
                            version.fetch_add(1, Ordering::SeqCst);
                        }
                        if body["snapshot_id"]
                            != json!(format!(
                                "original-version-{}",
                                version.load(Ordering::SeqCst)
                            ))
                        {
                            code = 409;
                            json!({"error":"original snapshot conflict"})
                        } else {
                            moves.fetch_add(1, Ordering::SeqCst);
                            let next = version.fetch_add(1, Ordering::SeqCst) + 1;
                            json!({"snapshot_id":format!("original-version-{next}")})
                        }
                    }
                    ("PUT", "/v1/me/player/pause") => {
                        playback["is_playing"] = json!(false);
                        code = 204;
                        Value::Null
                    }
                    ("PUT", "/v1/me/player/volume") => {
                        while hold_volume.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        code = 204;
                        Value::Null
                    }
                    ("POST", "/v1/me/player/previous") => {
                        while hold_previous.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        code = 204;
                        Value::Null
                    }
                    ("POST", "/v1/me/player/next") => {
                        while hold_next.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        code = 204;
                        Value::Null
                    }
                    ("PUT", "/v1/me/player/play")
                        if status.load(Ordering::SeqCst) == direct_owned_start::REJECT_FIRST_PLAY
                            && !rejected_direct_play =>
                    {
                        rejected_direct_play = true;
                        while hold_player.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        code = 404;
                        Value::Null
                    }
                    ("PUT", "/v1/me/player/play") => {
                        while hold_play.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        if status.load(Ordering::SeqCst) == 296 {
                            delayed_start = Some((
                                body["uris"][0].as_str().unwrap().chars().last().unwrap(),
                                2,
                            ));
                        } else {
                            playback["is_playing"] = json!(true);
                            if let Some(uri) = body["offset"]["uri"].as_str() {
                                playback["item"] = track(uri.chars().last().unwrap());
                                playback["context"] = json!({"uri":body["context_uri"]});
                                playback["progress_ms"] = body["position_ms"].clone();
                                playback["device"]["id"] = json!(query("device_id"));
                            } else if body["offset"]["position"].is_u64() {
                                positioned_start = true;
                                let kind = body["context_uri"].as_str().unwrap();
                                playback["item"] = track(if kind.starts_with("spotify:album:") { '4' } else { '6' });
                                playback["context"] = json!({"uri":kind});
                                playback["progress_ms"] = body["position_ms"].clone();
                                playback["device"]["id"] = json!(query("device_id"));
                            }
                        }
                        code = 204;
                        if status.load(Ordering::SeqCst) == 289 && body["offset"]["uri"].is_string()
                        {
                            code = 429;
                            playback = previous_playback.clone();
                        }
                        Value::Null
                    }
                    ("PUT", "/v1/me/player/seek") => {
                        playback["progress_ms"] =
                            json!(query("position_ms").parse::<u64>().unwrap());
                        code = 204;
                        Value::Null
                    }
                    _ => {
                        code = 204;
                        Value::Null
                    }
                };
                if status.load(Ordering::SeqCst) == 204 && url.path() == "/v1/me/player/devices" {
                    code = 200;
                    value = json!({"devices":[]});
                }
                if status.load(Ordering::SeqCst) == 299 {
                    code = 200;
                    value = Value::Null;
                }
                if status.load(Ordering::SeqCst) == 298 {
                    code = 200;
                }
                if status.load(Ordering::SeqCst) == 297
                    && method == "PUT"
                    && url.path() == "/v1/me/player"
                {
                    code = 403;
                }
                let mode = status.load(Ordering::SeqCst);
                if mode == 289 && method == "GET" {
                    code = 200;
                }
                if mode == 294 && method != "GET" {
                    code = 503;
                }
                if (290..=292).contains(&mode) {
                    code = if method == "GET" {
                        200
                    } else {
                        [400, 403, 404][usize::from(mode - 290)]
                    };
                    if code >= 400 {
                        playback = previous_playback;
                    }
                }
                if mode == 293 && method == "GET" && url.path() == "/v1/me/player" {
                    code = 500;
                }
                let rate_response = if code == 429 {
                    rate_response.lock().unwrap().clone()
                } else {
                    None
                };
                let body = if let Some(reply) = rate_response {
                    reply
                } else if status.load(Ordering::SeqCst) == 298 && method != "GET" {
                    "accepted".to_owned()
                } else if code == 204 {
                    String::new()
                } else {
                    serde_json::to_string(&value).unwrap()
                };
                let retry = retry_header
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|value| format!("Retry-After: {value}\r\n"))
                    .unwrap_or_default();
                let header = format!(
                    "HTTP/1.1 {code} Result\r\nContent-Type: application/json\r\n{retry}Location: http://127.0.0.1:1/no-redirect\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if code == 429 && hold_body.load(Ordering::SeqCst) {
                    if stream.write_all(header.as_bytes()).is_ok() {
                        let deadline = Instant::now() + Duration::from_secs(4);
                        while hold_body.load(Ordering::SeqCst)
                            && !stopping.load(Ordering::SeqCst)
                            && Instant::now() < deadline
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        let _ = stream.write_all(body.as_bytes());
                    }
                } else {
                    let _ = stream.write_all(format!("{header}{body}").as_bytes());
                }
            }
        });
        Self {
            base,
            requests,
            mode,
            delay,
            retry_after,
            rate_reply,
            hold_rate_body,
            hold_playback,
            empty_device_reads,
            hold_previous_response,
            hold_volume_response,
            hold_next_response,
            hold_play_response,
            fail_playback_reads,
            hold_positioned_playback,
            hold_resume_probe,
            stateful_spoken_library,
            playlist_version,
            change_on_items,
            change_on_move,
            applied_moves,
            stop,
            worker: Some(worker),
        }
    }
    fn provider(&self) -> SpotifyRemoteProvider {
        SpotifyRemoteProvider::with_base(self.base.clone()).unwrap()
    }
    fn records(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let address = self
            .base
            .trim_start_matches("http://")
            .trim_end_matches("/v1/");
        let _ = TcpStream::connect(address);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
#[track_caller]
fn wait(
    provider: &mut SpotifyRemoteProvider,
    accept: impl Fn(&RemoteUpdate) -> bool,
) -> RemoteUpdate {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(update) = provider.poll() {
            if accept(&update) {
                return update;
            }
            last = Some(update);
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("Remote fixture timed out: {last:?}")
}
fn connect(provider: &mut SpotifyRemoteProvider) {
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    wait(
        provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.connection==ProviderConnectionState::Connected && !s.devices.is_empty() && s.playback.as_ref().is_some_and(|p| p.upcoming.len() == 2)),
    );
}
#[test]
fn http_catalog_controls_and_reconciled_remote_clock() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p|p.state==PlaybackState::Paused)),
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Seek { position_ms: 2250 },
        })
        .unwrap();
    let result = wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p|p.position_ms==2250)),
    );
    if let RemoteUpdate::State(s) = result {
        let p = s.playback.unwrap();
        assert_eq!(p.upcoming.len(), 2);
        assert_eq!(p.device.as_ref().unwrap().device_type, "Computer");
        assert!(!p.capabilities.pcm_analysis);
        assert!(!p.capabilities.system_media_session);
        assert!(!p.transport.queue_edit);
    }
    for (query_id, kind, text) in [
        (1, RemoteCatalogKind::Search, "own fixture & author".into()),
        (2, RemoteCatalogKind::SavedTracks, String::new()),
        (3, RemoteCatalogKind::Playlists, String::new()),
        (4, RemoteCatalogKind::PlaylistItems, "a".repeat(22)),
    ] {
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id,
                    kind,
                    text,
                    offset: 10,
                },
            })
            .unwrap();
        let page = wait(
            &mut provider,
            |u| matches!(u,RemoteUpdate::Page(p) if p.query_id==query_id),
        );
        if let RemoteUpdate::Page(p) = page {
            assert!(p.issue.is_none());
            assert_eq!(p.offset, 10);
            assert_eq!(p.items.len() + p.collections.len(), 1);
        }
    }
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Transfer {
                device_id: "other-device".into(),
                operation_id: None,
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.connection==ProviderConnectionState::Connected),
    );
    let requests = server.records();
    assert!(requests.iter().all(|r| r.authorized));
    assert!(
        requests
            .iter()
            .any(|r| r.target.contains("position_ms=2250") && r.method == "PUT")
    );
    assert!(requests.iter().any(|r| r.target.starts_with("/v1/search?")
        && r.target.contains("limit=10")
        && r.target.contains("offset=10")
        && r.target.contains("%26")));
    assert!(
        requests
            .iter()
            .any(|r| r.target.starts_with("/v1/playlists/") && r.target.contains("/items?"))
    );
    assert!(requests.iter().any(|r| r.method == "PUT"
        && r.body["device_ids"][0] == "other-device"
        && r.body.get("play").is_none()));
}
#[test]
fn tracked_transfer_correlates_http_result_without_replacing_paused_playback() {
    let old: RemoteControl =
        serde_json::from_str(r#"{"kind":"transfer","device_id":"other-device"}"#).unwrap();
    assert_eq!(old.operation_id(), None);
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused)),
    );

    let transfer = |operation_id| RemoteControl::Transfer {
        device_id: "other-device".into(),
        operation_id: Some(operation_id),
    };
    assert!(supersedes(&transfer(18), &transfer(17)));
    for (operation_id, mode, rejected) in [(17, 297, true), (18, 200, false)] {
        server.mode.store(mode, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Control {
                control: transfer(operation_id),
            })
            .unwrap();
        let update = wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.control_result.as_ref().is_some_and(|r| r.operation_id == operation_id)),
        );
        let RemoteUpdate::State(state) = update else {
            unreachable!();
        };
        assert_eq!(state.control_result.unwrap().error.is_some(), rejected);
        assert_eq!(state.connection, ProviderConnectionState::Connected);
        let playback = state.playback.unwrap();
        assert_eq!(playback.state, PlaybackState::Paused);
        assert_eq!(playback.device.unwrap().id, "fixture-device");
    }
    let writes = server.records();
    let transfers = writes
        .iter()
        .filter(|r| r.method == "PUT" && r.target.trim_end_matches('?') == "/v1/me/player")
        .collect::<Vec<_>>();
    assert_eq!(transfers.len(), 2);
    assert!(
        transfers
            .iter()
            .all(|r| r.body == json!({"device_ids":["other-device"]}))
    );
    assert!(
        !writes
            .iter()
            .any(|r| r.target.starts_with("/v1/me/player/play"))
    );
}
#[test]
fn expired_token_stops_requests_until_host_supplies_a_new_credential() {
    let server = Server::new();
    server.mode.store(401, Ordering::SeqCst);
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.authentication_required&&s.playback.is_none()),
    );
    let count = server.records().len();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(count, server.records().len());
    server.mode.store(200, Ordering::SeqCst);
    connect(&mut provider);
}
#[test]
fn rate_limit_is_honored_and_no_device_is_a_real_empty_state() {
    let server = Server::new();
    server.mode.store(429, Ordering::SeqCst);
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.retry_after_seconds==Some(1)&&s.issue.as_ref().is_some_and(|e|e.code==ErrorCode::RateLimited)),
    );
    let count = server.records().len();
    provider.submit(RemoteAction::Refresh).unwrap();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(count, server.records().len());
    server.mode.store(204, Ordering::SeqCst);
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.connection==ProviderConnectionState::Connected&&s.playback.is_none()),
    );
}
#[test]
fn disconnect_cancels_followup_requests_and_drops_late_account_data() {
    let server = Server::new();
    server.delay.store(200, Ordering::SeqCst);
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.records().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(server.records().len(), 1);
    provider.submit(RemoteAction::Disconnect).unwrap();
    wait(
        &mut provider,
        |u| matches!(u,RemoteUpdate::State(s) if s.connection==ProviderConnectionState::Disconnected),
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(server.records().len(), 1);
    while let Some(update) = provider.poll() {
        assert!(
            matches!(update, RemoteUpdate::State(s) if s.connection==ProviderConnectionState::Disconnected&&s.playback.is_none()&&s.devices.is_empty())
        );
    }
}
#[test]
fn restrictions_are_capabilities_and_admission_is_bounded() {
    let mut value = frame();
    value["device"]["supports_volume"] = json!(false);
    value["actions"]["disallows"] =
        json!({"seeking":true,"skipping_next":true,"toggling_shuffle":true});
    let p = mapping::playback(&value);
    assert!(!p.capabilities.volume);
    assert!(!p.capabilities.seek);
    assert!(!p.transport.next);
    assert!(!p.transport.shuffle);
    assert!(p.capabilities.pause);
    let server = Server::new();
    let mut provider = server.provider();
    for _ in 0..32 {
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Next,
            })
            .unwrap();
    }
    assert!(
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Next
            })
            .is_err()
    );
    assert!(
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id: 1,
                    kind: RemoteCatalogKind::PlaylistItems,
                    text: "../../me".into(),
                    offset: 0
                }
            })
            .is_err()
    );
    assert!(
        !format!(
            "{:?}",
            RemoteAction::Credential {
                access_token: "sensitive-fixture".into()
            }
        )
        .contains("sensitive-fixture")
    );
    assert!(server.records().is_empty());
}
#[test]
fn permission_device_server_redirect_and_malformed_responses_are_honest_errors() {
    for (status, expected) in [
        (403, ErrorCode::PermissionLost),
        (404, ErrorCode::ProviderUnavailable),
        (500, ErrorCode::NetworkError),
        (302, ErrorCode::NetworkError),
        (299, ErrorCode::NetworkError),
    ] {
        let server = Server::new();
        server.mode.store(status, Ordering::SeqCst);
        let mut provider = server.provider();
        provider
            .submit(RemoteAction::Credential {
                access_token: "fixture-bearer".into(),
            })
            .unwrap();
        let result = wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.issue.is_some()),
        );
        if let RemoteUpdate::State(s) = result {
            assert_eq!(s.issue.unwrap().code, expected);
            assert_eq!(s.connection, ProviderConnectionState::Unavailable);
            assert!(s.playback.is_none());
        }
        assert_eq!(server.records().len(), 1); // Includes redirect: credentials never follow Location.
    }
}
#[test]
fn all_remote_commands_use_expected_http_requests_without_blind_retries() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let item = mapping::track(&track('8')).unwrap();
    for control in [
        RemoteControl::Play,
        RemoteControl::Next,
        RemoteControl::Previous,
        RemoteControl::Volume { volume: 0.45 },
        RemoteControl::Shuffle { enabled: true },
        RemoteControl::Repeat {
            mode: RepeatMode::One,
        },
        RemoteControl::Repeat {
            mode: RepeatMode::All,
        },
        RemoteControl::Repeat {
            mode: RepeatMode::Off,
        },
        RemoteControl::Start { item: item.clone() },
        RemoteControl::Enqueue { item },
    ] {
        let before = server.records().len();
        provider.submit(RemoteAction::Control { control }).unwrap();
        wait(&mut provider, |u| {
            matches!(u, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Connected)
                && server.records()[before..]
                    .iter()
                    .any(|request| matches!(request.method.as_str(), "PUT" | "POST"))
        });
    }
    let requests = server.records();
    assert!(
        requests
            .iter()
            .filter(|r| r.method == "PUT" || r.method == "POST")
            .all(|r| r.content_length.is_some()),
        "Spotify controls require explicit Content-Length, including zero-length bodies"
    );
    for (method, path, parameter) in [
        ("POST", "/v1/me/player/next", "device_id=fixture-device"),
        ("POST", "/v1/me/player/previous", "device_id=fixture-device"),
        ("PUT", "/v1/me/player/volume", "volume_percent=45"),
        ("PUT", "/v1/me/player/shuffle", "state=true"),
        ("PUT", "/v1/me/player/repeat", "state=track"),
        ("PUT", "/v1/me/player/repeat", "state=context"),
        ("PUT", "/v1/me/player/repeat", "state=off"),
        ("POST", "/v1/me/player/queue", "uri=spotify%3Atrack%3A"),
    ] {
        assert!(
            requests.iter().any(|r| r.method == method
                && r.target.starts_with(path)
                && r.target.contains(parameter)),
            "Missing expected remote operation"
        );
    }
    assert!(
        requests.iter().any(|r| r.method == "PUT"
            && r.body["uris"][0] == format!("spotify:track:{}", "8".repeat(22)))
    );
}
#[test]
fn background_polling_and_latest_search_are_bounded() {
    let server = Server::new();
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Visibility { visible: false })
        .unwrap();
    connect(&mut provider);
    let count = server.records().len();
    thread::sleep(Duration::from_millis(2200));
    assert_eq!(server.records().len(), count);
    // Unauthenticated work is coalesced before the worker can execute it.
    provider.submit(RemoteAction::Disconnect).unwrap();
    for query_id in 1..=100 {
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id,
                    kind: RemoteCatalogKind::Search,
                    text: "fixture".into(),
                    offset: 0,
                },
            })
            .unwrap();
    }
    assert_eq!(
        provider
            .shared
            .0
            .lock()
            .unwrap()
            .query
            .as_ref()
            .unwrap()
            .query_id,
        100
    );
}

#[test]
fn successful_control_ack_does_not_require_a_json_body() {
    let server = Server::new();
    server.mode.store(298, Ordering::SeqCst);
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    assert!(api.control(&RemoteControl::Pause, None).is_ok());
    assert_eq!(server.records().len(), 1);
    assert_eq!(server.records()[0].content_length, Some(0));
}

#[test]
fn owned_playback_transfers_then_starts_on_the_explicit_device() {
    let server = Server::new();
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    let item = mapping::track(&track('1')).unwrap();
    let control = RemoteControl::StartOnDevice {
        operation_id: None,
        item,
        device_id: "auvra-device".into(),
    };
    assert!(api.control(&control, Some("stale-device")).is_ok());
    let records = server.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].target.trim_end_matches('?'), "/v1/me/player");
    assert_eq!(
        records[0].body,
        json!({"device_ids":["auvra-device"],"play":false})
    );
    assert_eq!(
        records[1].target,
        "/v1/me/player/play?device_id=auvra-device"
    );
    assert_eq!(
        records[1].body,
        json!({"uris":[format!("spotify:track:{}", "1".repeat(22))]})
    );
    server.mode.store(297, Ordering::SeqCst);
    assert!(api.control(&control, Some("stale-device")).is_err());
    let after = server.records();
    assert_eq!(
        after.len(),
        3,
        "A failed transfer must never play on the previous device"
    );
    assert_eq!(after[2].target.trim_end_matches('?'), "/v1/me/player");
}

// Concurrent original HTTP fixture: deliberately stall an auxiliary endpoint while
// accepting transport requests. A serial fixture would hide head-of-line blocking.
struct SlowFixture {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
struct QueueReply {
    payload: Value,
    hold: Option<Arc<AtomicBool>>,
}
impl SlowFixture {
    fn new(slow_path: &'static str, status: u16) -> Self {
        Self::with_playback_and_queue(
            slow_path,
            status,
            Arc::new(Mutex::new(frame())),
            Arc::default(),
        )
    }
    fn with_playback_and_queue(
        slow_path: &'static str,
        status: u16,
        playback: Arc<Mutex<Value>>,
        queue: Arc<Mutex<VecDeque<QueueReply>>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let records = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let playing = Arc::new(AtomicBool::new(true));
        let worker = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let records = records.clone();
                let playing = playing.clone();
                let playback = playback.clone();
                let queue = queue.clone();
                let stopping = stopping.clone();
                clients.push(thread::spawn(move || {
                    let mut bytes = vec![]; let mut buffer = [0; 4096];
                    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        let count = stream.read(&mut buffer).unwrap_or(0); if count == 0 { return; } bytes.extend_from_slice(&buffer[..count]);
                    }
                    let text = String::from_utf8_lossy(&bytes); let mut line = text.lines().next().unwrap().split_whitespace();
                    let method = line.next().unwrap(); let target = line.next().unwrap(); let path = target.split('?').next().unwrap();
                    records.lock().unwrap().push(path.to_owned());
                    if path == slow_path { thread::sleep(Duration::from_millis(2000)); }
                    let (code, value) = match (method, path) {
                        ("GET", "/v1/me/player") => { let mut value = playback.lock().unwrap().clone(); value["is_playing"] = json!(playing.load(Ordering::SeqCst)); (200, value) },
                        ("GET", "/v1/me/player/devices") => (if path == slow_path { status } else { 200 }, json!({"devices":[playback.lock().unwrap()["device"].clone()]})),
                        ("GET", "/v1/me/player/queue") => {
                            let reply = queue.lock().unwrap().pop_front();
                            let value = if let Some(reply) = reply {
                                if let Some(hold) = reply.hold {
                                    let deadline = Instant::now() + Duration::from_secs(4);
                                    while hold.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                                        thread::sleep(Duration::from_millis(5));
                                    }
                                }
                                reply.payload
                            } else {
                                json!({"currently_playing":playback.lock().unwrap()["item"],"queue":[track('2')]})
                            };
                            (if path == slow_path { status } else { 200 }, value)
                        },
                        ("GET", "/v1/search") => (if path == slow_path { status } else { 200 }, json!({"tracks":{"items":[track('3')],"total":1}})),
                        ("PUT", "/v1/me/player/pause") => { playing.store(false, Ordering::SeqCst); (204, Value::Null) },
                        _ => (204, Value::Null),
                    };
                    let body = if code == 204 { String::new() } else { value.to_string() };
                    let response = format!("HTTP/1.1 {code} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes());
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn wait_for(&self, path: &str) {
        self.wait_for_count(path, 1);
    }
    fn wait_for_count(&self, path: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if self
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.as_str() == path)
                .count()
                >= count
            {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("Auxiliary fixture did not receive {path}");
    }
}
impl Drop for SlowFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
#[test]
fn playback_http_honors_flat_and_legacy_action_restrictions() {
    let mut raw = frame();
    raw["actions"] = json!({
        "resuming":true, "pausing":true, "seeking":true,
        "skipping_next":true, "skipping_prev":true,
        "toggling_shuffle":true, "toggling_repeat_context":true
    });
    let raw = Arc::new(Mutex::new(raw));
    let server = SlowFixture::with_playback_and_queue("", 200, raw.clone(), Arc::default());
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    let read = || {
        api.playback()
            .map_err(|failure| failure.error)
            .unwrap()
            .unwrap()
    };

    let playback = read();
    assert!(!playback.capabilities.play && !playback.capabilities.pause);
    assert!(
        !playback.capabilities.seek && !playback.transport.next && !playback.transport.previous
    );
    assert!(!playback.transport.shuffle && !playback.transport.repeat);
    assert!(playback.capabilities.volume && playback.transport.queue_add);

    raw.lock().unwrap()["actions"] = json!({
        "seeking":false, "skipping_next":true, "pausing":"true",
        "disallows":{"seeking":true, "skipping_next":false, "toggling_repeat_track":true}
    });
    let playback = read();
    assert!(!playback.capabilities.seek && !playback.transport.next && !playback.transport.repeat);
    assert!(playback.capabilities.play && playback.capabilities.pause);
    assert!(playback.transport.previous && playback.transport.shuffle);

    raw.lock()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("actions");
    let playback = read();
    assert!(
        playback.capabilities.play && playback.capabilities.pause && playback.capabilities.seek
    );
    assert!(playback.transport.next && playback.transport.previous);
    assert!(playback.transport.shuffle && playback.transport.repeat);
}

#[test]
fn empty_device_discovery_retries_without_the_next_playback_poll() {
    let server = Server::new();
    server.empty_device_reads.store(1, Ordering::SeqCst);
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Visibility { visible: false })
        .unwrap();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Connected && !s.devices.is_empty()),
    );
    let requests = server.records();
    let count = |prefix: &str| {
        requests
            .iter()
            .filter(|request| request.target.starts_with(prefix))
            .count()
    };
    assert_eq!(count("/v1/me/player?"), 1);
    assert_eq!(count("/v1/me/player/devices"), 2);
}

#[test]
fn empty_device_discovery_retries_are_bounded_and_coalesce_other_requests() {
    let start = Instant::now();
    let mut mailbox = Mailbox {
        auxiliary_token: Some("fixture-bearer".into()),
        ..Mailbox::default()
    };
    mailbox.request_device_discovery(start, true);
    assert!(mailbox.device_discovery.start(start));
    mailbox.request_device_discovery(start, true);
    assert!(!mailbox.device_discovery.requested);
    mailbox.device_discovery.finish(start, Some(true));
    let mut now = start;
    for ms in DeviceDiscovery::EMPTY_BACKOFF_MS {
        mailbox.request_device_discovery(now, true);
        mailbox.request_device_discovery(now, false);
        assert!(!mailbox.device_discovery.requested);
        now += Duration::from_millis(ms);
        assert!(
            !mailbox
                .device_discovery
                .start(now - Duration::from_millis(1))
        );
        assert!(mailbox.device_discovery.start(now));
        mailbox.device_discovery.finish(now, Some(true));
    }
    assert!(mailbox.device_discovery.retry_at.is_none());
    let periodic = mailbox.device_discovery.periodic_at.unwrap();
    mailbox.request_device_discovery(periodic - Duration::from_millis(1), false);
    assert!(
        !mailbox
            .device_discovery
            .start(periodic - Duration::from_millis(1))
    );
    mailbox.request_device_discovery(periodic, false);
    assert!(mailbox.device_discovery.start(periodic));
    mailbox.device_discovery.finish(periodic, Some(true));
    assert!(mailbox.device_discovery.retry_at.is_none());

    mailbox.request_device_discovery(periodic, true);
    assert!(mailbox.device_discovery.start(periodic));
    mailbox.device_discovery.finish(periodic, Some(false));
    mailbox.request_device_discovery(periodic, true);
    assert!(mailbox.device_discovery.start(periodic));
    mailbox.device_discovery.finish(periodic, Some(true));
    assert!(mailbox.device_discovery.retry_at.is_some());
    mailbox.limit_requests(30, periodic);
    assert!(mailbox.device_discovery.retry_at.is_none());
    mailbox.request_device_discovery(periodic, true);
    assert!(!mailbox.device_discovery.requested);
}

#[test]
fn explicit_refresh_starts_device_discovery_during_a_pending_playback_read() {
    for action in [RemoteAction::Activate, RemoteAction::Refresh] {
        let playback = Arc::new(Mutex::new(frame()));
        let server = SlowFixture::with_playback_and_queue(
            "/v1/me/player",
            200,
            playback.clone(),
            Arc::default(),
        );
        let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
        provider
            .submit(RemoteAction::Visibility { visible: false })
            .unwrap();
        provider
            .submit(RemoteAction::Credential {
                access_token: "fixture-bearer".into(),
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Connected && !s.devices.is_empty()),
        );
        playback.lock().unwrap()["device"]["name"] = json!("Updated fixture output");
        let initial = server.requests.lock().unwrap().len();
        provider.submit(action).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let requests = server.requests.lock().unwrap();
            let current = &requests[initial..];
            if current.iter().any(|path| path == "/v1/me/player/devices")
                && current.iter().any(|path| path == "/v1/me/player")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Expected discovery during the two-second playback read"
            );
            drop(requests);
            thread::sleep(Duration::from_millis(5));
        }
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.devices.iter().any(|device| device.name == "Updated fixture output")),
        );
        assert_eq!(
            server.requests.lock().unwrap()[initial..]
                .iter()
                .filter(|path| path.as_str() == "/v1/me/player/devices")
                .count(),
            1
        );
    }
}

#[test]
fn slow_queue_discovery_and_search_never_delay_playback_or_pause() {
    for path in ["/v1/me/player/queue", "/v1/me/player/devices", "/v1/search"] {
        let server = SlowFixture::new(path, 200);
        let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
        let started = Instant::now();
        provider
            .submit(RemoteAction::Credential {
                access_token: "fixture-bearer".into(),
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.playback.is_some() && s.connection == ProviderConnectionState::Connected),
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Initial playback waited for {path}"
        );
        if path == "/v1/search" {
            provider
                .submit(RemoteAction::Query {
                    query: RemoteQuery {
                        query_id: 7,
                        kind: RemoteCatalogKind::Search,
                        text: "original fixture".into(),
                        offset: 0,
                    },
                })
                .unwrap();
        }
        server.wait_for(path);
        let started = Instant::now();
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Pause,
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused)),
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Pause was blocked by {path}"
        );
    }
}
#[test]
fn a_failed_queue_endpoint_does_not_disable_working_playback() {
    for status in [403, 500] {
        let server = SlowFixture::new("/v1/me/player/queue", status);
        let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
        provider
            .submit(RemoteAction::Credential {
                access_token: "fixture-bearer".into(),
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.playback.is_some()),
        );
        server.wait_for("/v1/me/player/queue");
        thread::sleep(Duration::from_millis(2100));
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Pause,
            })
            .unwrap();
        let result = wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused)),
        );
        let RemoteUpdate::State(state) = result else {
            unreachable!()
        };
        assert_eq!(state.connection, ProviderConnectionState::Connected);
        assert!(state.issue.is_none());
    }
}
#[test]
fn previous_response_timeout_is_unconfirmed_and_does_not_repeat_the_write() {
    let server = Server::new();
    server.delay.store(5_250, Ordering::SeqCst);
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());

    let Err(failure) = api.control(&RemoteControl::Previous, Some("fixture-device")) else {
        panic!("the response should time out after Spotify receives the write");
    };
    assert_eq!(failure.status, 0);
    assert_eq!(
        failure.error.message,
        "Spotify did not confirm the previous-track command."
    );
    let records = server.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "POST");
    assert_eq!(
        records[0].target,
        "/v1/me/player/previous?device_id=fixture-device"
    );
}
#[test]
fn starting_another_track_on_the_active_device_does_not_transfer_or_pause_it() {
    let server = Server::new();
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    assert!(
        api.control(
            &RemoteControl::StartOnDevice {
                operation_id: None,
                item: mapping::track(&track('1')).unwrap(),
                device_id: "auvra-device".into(),
            },
            Some("auvra-device"),
        )
        .is_ok()
    );
    let records = server.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].target,
        "/v1/me/player/play?device_id=auvra-device"
    );
}
#[test]
fn rapid_settings_and_track_selection_are_coalesced_without_losing_discrete_skips() {
    let server = Server::new();
    let mut provider = server.provider();
    for value in 0..100 {
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Volume {
                    volume: value as f32 / 100.0,
                },
            })
            .unwrap();
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: value },
            })
            .unwrap();
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::StartOnDevice {
                    operation_id: None,
                    item: mapping::track(&track('1')).unwrap(),
                    device_id: format!("device-{value}"),
                },
            })
            .unwrap();
    }
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    let mailbox = provider.shared.0.lock().unwrap();
    assert_eq!(mailbox.controls.len(), 4); // latest volume/start plus both explicit skips; old seek belongs to old track
    assert!(mailbox.controls.iter().any(
        |c| matches!(&c.control, RemoteControl::Volume { volume } if (*volume-0.99).abs() < 0.001)
    ));
    assert!(mailbox.controls.iter().any(
        |c| matches!(&c.control, RemoteControl::StartOnDevice { device_id, .. } if device_id == "device-99")
    ));
    assert!(server.records().is_empty());
}

#[test]
fn an_inflight_reply_cannot_restore_data_after_authorization_is_revoked() {
    let server = Server::new();
    server.delay.store(150, Ordering::SeqCst);
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared.clone()).unwrap();
    api.token = Some("fixture-bearer".into());
    let request = thread::spawn(move || api.playback().err().map(|error| error.status));
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.records().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    shared.0.lock().unwrap().authorization_revoked = true;
    assert_eq!(request.join().unwrap(), Some(401));
    publish(
        &shared,
        0,
        RemoteUpdate::Page(RemotePage {
            position_edits: false,
            related_collections: vec![],
            detail: false,
            item_positions: vec![],
            next_cursor: None,
            provider: "spotify".into(),
            query_id: 1,
            offset: 0,
            total: 1,
            items: vec![mapping::track(&track('1')).unwrap()],
            collections: vec![],
            spoken_progress: vec![],
            recent_plays: vec![],
            issue: None,
        }),
    );
    assert!(shared.0.lock().unwrap().page.is_none());
    publish(
        &shared,
        0,
        RemoteUpdate::State(Box::new(disconnected(true))),
    );
    assert!(
        shared
            .0
            .lock()
            .unwrap()
            .state
            .as_ref()
            .unwrap()
            .authentication_required
    );
}

#[test]
fn background_track_start_reconciles_eventual_consistency_without_replaying_the_command() {
    let server = Server::new();
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Visibility { visible: false })
        .unwrap();
    connect(&mut provider);
    server.mode.store(296, Ordering::SeqCst);
    let item = mapping::track(&track('8')).unwrap();
    let id = item.id.clone();
    let before = Instant::now();
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartOnDevice {
                operation_id: None,
                item,
                device_id: "fixture-device".into(),
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::State(state) if state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Playing && frame.current.as_ref().is_some_and(|item| item.id == id))),
    );
    assert!(
        before.elapsed() < Duration::from_secs(3),
        "Background playback waited for the ordinary 15-second poll"
    );
    let records = server.records();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.method == "PUT" && r.target.starts_with("/v1/me/player/play"))
            .count(),
        1,
        "A delayed observation must never repeat Play"
    );
    assert!(
        records
            .iter()
            .filter(|r| r.method == "GET" && r.target.trim_end_matches('?') == "/v1/me/player")
            .count()
            <= 5,
        "Reconciliation must remain bounded"
    );
}

fn collection_fixture(kind: &str) -> Value {
    let mut value = json!({"id":"a".repeat(22),"type":kind,"name":format!("Original {kind}"),"description":"Original description","snapshot_id":"original-version-1","artists":[{"name":"Original creator"}],"publisher":"Original publisher","images":[]});
    if kind == "audiobook" {
        value.as_object_mut().unwrap().remove("artists");
        value["authors"] = json!([{"name":"Original author"}]);
        value["narrators"] = json!([{"name":"Original narrator"}]);
    }
    value
}
fn spoken_fixture(kind: &str) -> Value {
    json!({"id":"e".repeat(22),"type":kind,"name":format!("Original {kind}"),"duration_ms":120000,"is_playable":true,"images":[],"show":{"id":"s".repeat(22),"name":"Original show","publisher":"Original publisher"},"audiobook":{"id":"b".repeat(22),"name":"Original book","authors":[{"name":"Original author"}],"narrators":[{"name":"Original narrator"}]}})
}
fn spoken_progress_fixture(kind: &str, id: char, point: Value) -> Value {
    let mut row = spoken_fixture(kind);
    row["id"] = json!(id.to_string().repeat(22));
    row["resume_point"] = point;
    row
}
fn fixture_api(server: &Server) -> Api {
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    api
}
#[test]
fn search_all_content_types_preserves_entity_identity() {
    use RemoteCatalogKind::*;
    let server = Server::new();
    let mut api = fixture_api(&server);
    for (index, (kind, expected, playable)) in [
        (Search, "track", true),
        (SearchAlbums, "album", false),
        (SearchArtists, "artist", false),
        (SearchPlaylists, "playlist", false),
        (SearchShows, "show", false),
        (SearchEpisodes, "episode", true),
        (SearchAudiobooks, "audiobook", false),
    ]
    .into_iter()
    .enumerate()
    {
        let page = api
            .catalog(&RemoteQuery {
                query_id: index as u64 + 1,
                kind,
                text: "original & русский".into(),
                offset: 10,
            })
            .unwrap_or_else(|_| panic!("catalog {expected}"));
        assert!(page.issue.is_none());
        if playable {
            assert_eq!(page.items.len(), 1);
            assert!(
                page.items[0]
                    .id
                    .as_str()
                    .starts_with(&format!("spotify:{expected}:"))
            );
        } else {
            assert_eq!(page.collections.len(), 1);
            assert!(page.collections[0].url.contains(&format!("/{expected}/")));
        }
    }
    let records = server.requests.lock().unwrap();
    for expected in [
        "track",
        "album",
        "artist",
        "playlist",
        "show",
        "episode",
        "audiobook",
    ] {
        assert!(
            records
                .iter()
                .any(|r| r.target.contains(&format!("type={expected}"))
                    && r.target.contains("offset=10")
                    && r.target.contains("%26"))
        );
    }
}
#[test]
fn mixed_search_reads_one_page_of_all_types_and_omits_invalid_rows() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let text = "original & русский";
    let page = api
        .catalog(&RemoteQuery {
            query_id: 51,
            kind: RemoteCatalogKind::SearchAll,
            text: text.into(),
            offset: 10,
        })
        .unwrap_or_else(|_| panic!("mixed catalog page"));

    assert_eq!(page.query_id, 51);
    assert_eq!(page.offset, 10);
    assert_eq!(page.total, 47, "The largest group determines pagination");
    assert!(!page.detail && !page.position_edits && page.item_positions.is_empty());
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.id.as_str().split(':').nth(1).unwrap())
            .collect::<Vec<_>>(),
        ["track", "episode"]
    );
    assert_eq!(
        page.spoken_progress,
        [SpokenProgress {
            media_id: page.items[1].id.clone(),
            position_ms: 23_000,
            fully_played: false,
        }]
    );
    assert_eq!(
        page.collections
            .iter()
            .map(|collection| collection.kind)
            .collect::<Vec<_>>(),
        [
            RemoteEntityKind::Album,
            RemoteEntityKind::Artist,
            RemoteEntityKind::Playlist,
            RemoteEntityKind::Show,
            RemoteEntityKind::Audiobook,
        ]
    );
    let issue = page.issue.unwrap();
    assert_eq!(issue.code, ErrorCode::TrackUnavailable);
    assert_eq!(issue.message, "3 unavailable items were omitted.");

    let records = server.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].method, "GET");
    assert!(records[0].authorized);
    let url = reqwest::Url::parse(&format!("http://fixture{}", records[0].target)).unwrap();
    assert_eq!(url.path(), "/v1/search");
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["q"], text);
    assert_eq!(
        query["type"],
        "track,album,artist,playlist,show,episode,audiobook"
    );
    assert_eq!(query["limit"], "10");
    assert_eq!(query["offset"], "10");
}

#[test]
fn spoken_progress_follows_catalog_identity_and_stays_out_of_media_items() {
    use RemoteCatalogKind::*;
    let server = Server::new();
    let mut api = fixture_api(&server);
    let mut request = |kind, id: &str| {
        api.catalog(&RemoteQuery {
            query_id: 1,
            kind,
            text: id.repeat(22),
            offset: 0,
        })
        .unwrap_or_else(|_| panic!("spoken catalog route"))
    };
    let episodes = request(ShowEpisodes, "p");
    assert_eq!(episodes.items.len(), 10);
    assert_eq!(
        episodes.spoken_progress,
        [
            SpokenProgress {
                media_id: episodes.items[1].id.clone(),
                position_ms: 23_000,
                fully_played: false,
            },
            SpokenProgress {
                media_id: episodes.items[6].id.clone(),
                position_ms: 120_000,
                fully_played: true,
            },
        ]
    );
    let serialized = serde_json::to_value(&episodes).unwrap();
    assert_eq!(
        serialized["spoken_progress"][0]["media_id"],
        episodes.items[1].id.as_str()
    );
    assert!(serialized["items"][1].get("resume_point").is_none());
    assert!(serialized["items"][1].get("spoken_progress").is_none());

    for (kind, id, position_ms, fully_played) in [
        (SavedEpisodes, "", 23_000, false),
        (Episode, "e", 23_000, false),
        (AudiobookChapters, "p", 41_000, false),
        (Chapter, "e", 120_000, true),
    ] {
        let page = request(kind, id);
        assert_eq!(
            page.spoken_progress,
            [SpokenProgress {
                media_id: page.items[0].id.clone(),
                position_ms,
                fully_played,
            }]
        );
    }
    let missing_point = request(ShowEpisodes, "a");
    let wrong_detail_id = request(Chapter, "a");
    for page in [missing_point, wrong_detail_id] {
        assert!(page.spoken_progress.is_empty());
        let serialized = serde_json::to_value(page).unwrap();
        assert!(serialized.get("spoken_progress").is_none());
    }
    assert_eq!(server.records().len(), 7);
}
#[test]
fn artwork_mapping_preserves_custom_cover_cdns_and_rejects_foreign_origins() {
    for host in [
        "i.scdn.co",
        "mosaic.scdn.co",
        "image-cdn-fa.spotifycdn.com",
        "image-cdn-ak.spotifycdn.com",
    ] {
        let url = format!("https://{host}/image/original-cover-fixture");
        assert_eq!(mapping::image(&json!([{"url": url}])), Some(url));
    }
    for url in [
        "http://image-cdn-fa.spotifycdn.com/image/x",
        "https://image-cdn-fa.spotifycdn.com.evil.invalid/image/x",
        "https://image-cdn-ak.spotifycdn.com@evil.invalid/image/x",
        "https://user@image-cdn-fa.spotifycdn.com/image/x",
        "https://image-cdn-fa.spotifycdn.com:444/image/x",
        "https://image-cdn-fa.spotifycdn.com/other/x",
        "https://evil.invalid/image/x",
    ] {
        assert!(mapping::image(&json!([{"url": url}])).is_none());
    }
    let oversized = format!("https://i.scdn.co/image/{}", "a".repeat(4096));
    assert!(mapping::image(&json!([{"url": oversized}])).is_none());
}
#[test]
fn saved_history_top_and_detail_catalog_routes_are_typed() {
    use RemoteCatalogKind::*;
    let server = Server::new();
    let mut api = fixture_api(&server);
    for (kind, text, collections) in [
        (SavedAlbums, "", true),
        (SavedShows, "", true),
        (SavedAudiobooks, "", true),
        (SavedEpisodes, "", false),
        (TopTracks, "short_term", false),
        (TopArtists, "long_term", true),
        (Recent, "", false),
        (FollowedArtists, "", true),
        (AlbumTracks, "a", false),
        (ArtistAlbums, "a", true),
        (ShowEpisodes, "a", false),
        (AudiobookChapters, "a", false),
        (PlaylistInfo, "a", true),
        (PlaylistImages, "a", true),
        (Album, "a", true),
        (Artist, "a", true),
        (Show, "a", true),
        (Audiobook, "a", true),
        (Track, "a", false),
        (Episode, "a", false),
        (Chapter, "e", false),
        (Profile, "", true),
    ] {
        let text = if matches!(text, "a" | "e") {
            text.repeat(22)
        } else {
            text.into()
        };
        let cover_query = matches!(&kind, PlaylistImages);
        let page = api
            .catalog(&RemoteQuery {
                query_id: 1,
                kind,
                text,
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("catalog route"));
        assert!(page.issue.is_none());
        if cover_query {
            assert_eq!(
                page.collections[0].artwork.as_deref(),
                Some("https://image-cdn-fa.spotifycdn.com/image/original-cover-fixture")
            );
        }
        assert_eq!(
            if collections {
                page.collections.len()
            } else {
                page.items.len()
            },
            1
        );
    }
    let records = server.requests.lock().unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.target.contains("time_range=short_term"))
    );
    assert!(
        records
            .iter()
            .any(|r| r.target.contains("time_range=long_term"))
    );
}
#[test]
fn cursor_pages_use_spotify_cursors_without_fabricating_offsets() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let page = api
        .catalog(&RemoteQuery {
            query_id: 1,
            kind: RemoteCatalogKind::FollowedArtists,
            text: "a".repeat(22),
            offset: 10,
        })
        .unwrap_or_else(|_| panic!("artists"));
    assert_eq!(page.next_cursor, Some("b".repeat(22)));
    let page = api
        .catalog(&RemoteQuery {
            query_id: 2,
            kind: RemoteCatalogKind::Recent,
            text: "1740000000001".into(),
            offset: 10,
        })
        .unwrap_or_else(|_| panic!("recent"));
    assert_eq!(page.next_cursor.as_deref(), Some("1740000000000"));
    let records = server.requests.lock().unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.target.contains("after=aaaa") && !r.target.contains("offset="))
    );
    assert!(
        records
            .iter()
            .any(|r| r.target.contains("before=1740000000001") && !r.target.contains("offset="))
    );
}
#[test]
fn history_keeps_per_play_timestamps_and_context_after_omitting_an_unavailable_row() {
    let server = Server::with_catalog_data(
        json!({"id":"original-test-user"}),
        json!({"items":[
            {"track":track('4'),"played_at":"2026-11-05T17:40:07.250Z","context":{"uri":format!("spotify:playlist:{}", "p".repeat(22))}},
            {"track":null,"played_at":"2026-11-05T17:39:00Z","context":{"uri":format!("spotify:artist:{}", "a".repeat(22))}},
            {"track":track('4'),"played_at":"2026-11-05T17:37:04Z","context":{"uri":format!("spotify:album:{}", "b".repeat(22))}},
            {"track":track('5'),"played_at":"invalid time","context":{"uri":format!("spotify:artist:{}", "a".repeat(22))}},
            {"track":track('6'),"played_at":"2026-11-04T23:12:00+01:00","context":{"uri":"https://example.test/collection"}},
            {"track":track('7'),"played_at":"2026-11-05T17:25:15Z\n","context":{"uri":"spotify:playlist:../unsafe"}},
            {"track":track('8'),"context":{"uri":format!("spotify:show:{}", "s".repeat(22))}}
        ]}),
    );
    let mut api = fixture_api(&server);
    for (kind, offset) in [
        (RemoteCatalogKind::Recent, 10),
        (RemoteCatalogKind::HomeRecent, 0),
    ] {
        let page = api
            .catalog(&RemoteQuery {
                query_id: 1,
                kind,
                text: String::new(),
                offset,
            })
            .unwrap_or_else(|_| panic!("history"));
        assert_eq!(page.items.len(), 6);
        assert_eq!(page.items[0].id, page.items[1].id);
        assert_eq!(
            page.item_positions,
            [
                offset,
                offset + 2,
                offset + 3,
                offset + 4,
                offset + 5,
                offset + 6
            ]
        );
        assert_eq!(
            page.issue.as_ref().unwrap().code,
            ErrorCode::TrackUnavailable
        );
        assert_eq!(
            page.recent_plays,
            [
                RecentPlay {
                    item_index: 0,
                    played_at: Some("2026-11-05T17:40:07.250Z".into()),
                    context_uri: Some(format!("spotify:playlist:{}", "p".repeat(22)))
                },
                RecentPlay {
                    item_index: 1,
                    played_at: Some("2026-11-05T17:37:04Z".into()),
                    context_uri: Some(format!("spotify:album:{}", "b".repeat(22)))
                },
                RecentPlay {
                    item_index: 2,
                    played_at: None,
                    context_uri: Some(format!("spotify:artist:{}", "a".repeat(22)))
                },
                RecentPlay {
                    item_index: 3,
                    played_at: Some("2026-11-04T23:12:00+01:00".into()),
                    context_uri: None
                },
                RecentPlay {
                    item_index: 5,
                    played_at: None,
                    context_uri: Some(format!("spotify:show:{}", "s".repeat(22)))
                },
            ]
        );
    }
    let top = api
        .catalog(&RemoteQuery {
            query_id: 2,
            kind: RemoteCatalogKind::TopTracks,
            text: String::new(),
            offset: 0,
        })
        .unwrap_or_else(|_| panic!("top"));
    assert!(
        serde_json::to_value(top)
            .unwrap()
            .get("recent_plays")
            .is_none()
    );
}
#[test]
fn spoken_items_are_playable_without_claiming_pcm_or_local_media_session() {
    for kind in ["episode", "chapter"] {
        let item = mapping::track(&spoken_fixture(kind)).unwrap();
        assert!(item.capabilities.play);
        assert!(!item.capabilities.pcm_analysis);
        assert!(!item.capabilities.system_media_session);
        assert_eq!(
            mapping::track_uri(&item).unwrap(),
            format!("spotify:episode:{}", "e".repeat(22))
        );
    }
    let mut value = frame();
    value["item"] = spoken_fixture("episode");
    assert!(
        mapping::playback(&value)
            .current
            .unwrap()
            .id
            .as_str()
            .starts_with("spotify:episode:")
    );
}

#[test]
fn chapter_transport_uses_episode_uri_without_changing_catalog_identity_or_queue_policy() {
    let server = Server::new();
    let api = fixture_api(&server);
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let uri = format!("spotify:episode:{}", "e".repeat(22));
    assert_eq!(
        chapter.id.as_str(),
        format!("spotify:chapter:{}", "e".repeat(22))
    );
    assert!(
        matches!(&chapter.location, Some(MediaLocation::Remote { uri: value, url, .. }) if value == chapter.id.as_str() && url.contains("/chapter/"))
    );
    assert!(mapping::queue_uri(&chapter).is_err());
    assert!(
        RemoteSelection {
            provider: "spotify".into(),
            account_id: "original-account".into(),
            stable_account_id: None,
            current: chapter.clone(),
            position_ms: 30000,
            context_uri: None
        }
        .validate()
        .is_ok(),
        "Stored locator must retain logical chapter identity; only the transport converts its URI"
    );
    api.control(
        &RemoteControl::StartOnDevice {
            item: chapter.clone(),
            device_id: "fixture-device".into(),
            operation_id: Some(91),
        },
        Some("fixture-device"),
    )
    .unwrap_or_else(|_| panic!("chapter start"));
    assert_eq!(server.records()[0].body, json!({"uris":[uri]}));
    let mut clock = output_clock(1);
    clock.control_uptime_ms = Some(1000);
    let command = RemoteControl::StartOnDevice {
        item: chapter.clone(),
        device_id: clock.device_id.clone(),
        operation_id: Some(92),
    };
    assert_eq!(
        owned_commands::convert(&command, Some(&clock)),
        Some(RemoteOutputControl::StartUri {
            uri: uri.clone(),
            operation_id: 92
        })
    );

    let mut observed = output_base(1).playback.unwrap();
    let mut item = chapter;
    item.id = MediaId::parse(uri).unwrap();
    observed.current = Some(item);
    let confirmation = ControlConfirmation {
        control: command,
        previous: None,
        until: Instant::now(),
        attempts: 0,
    };
    assert!(confirmation.matches(Some(&observed)));
    observed.device.as_mut().unwrap().id = "other-device".into();
    assert!(!confirmation.matches(Some(&observed)));
}

#[test]
fn chapter_episode_identity_alias_is_exact_and_preserves_output_binding() {
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let episode = MediaId::parse(format!("spotify:episode:{}", "e".repeat(22))).unwrap();
    assert!(mapping::same_playback_id(&chapter.id, &episode));
    assert!(mapping::same_playback_id(&episode, &chapter.id));
    for other in [
        format!("spotify:episode:{}", "d".repeat(22)),
        format!("spotify:track:{}", "e".repeat(22)),
        format!("other:episode:{}", "e".repeat(22)),
    ] {
        assert!(!mapping::same_playback_id(
            &chapter.id,
            &MediaId::parse(other).unwrap()
        ));
    }
    assert!(!mapping::same_playback_id(
        &MediaId::parse("spotify:chapter:short").unwrap(),
        &MediaId::parse("spotify:episode:short").unwrap()
    ));
    let now = Instant::now();
    let mut base = output_base(1);
    base.playback.as_mut().unwrap().current = Some(chapter.clone());
    let mut output = output_clock::OutputProjection::default();
    output.ingest(base, now);
    let mut clock = output_clock(1);
    clock.media_id = Some(episode);
    clock.control_uptime_ms = Some(1000);
    assert!(!output.observe(Some(clock.clone()), now).unwrap());
    let projected = output.take(now).unwrap();
    assert!(projected.owned_output_observed && projected.owned_output_control);
    assert_eq!(projected.playback.unwrap().current.unwrap(), chapter);
    clock.sequence = 2;
    clock.device_id = "other-device".into();
    output.observe(Some(clock), now).unwrap();
    let foreign = output.take(now).unwrap();
    assert!(!foreign.owned_output_observed && !foreign.owned_output_control);
}

#[test]
fn audiobook_authors_survive_search_details_and_chapter_relationships() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    for kind in [
        RemoteCatalogKind::SearchAudiobooks,
        RemoteCatalogKind::SavedAudiobooks,
        RemoteCatalogKind::Audiobook,
    ] {
        let page = api
            .catalog(&RemoteQuery {
                query_id: 1,
                kind,
                text: "a".repeat(22),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("audiobook catalog"));
        assert_eq!(page.collections[0].subtitle, "Original author");
    }
    let mut value = spoken_fixture("chapter");
    value["audiobook"]["authors"] = json!([
        {"name":" Original author "}, {"name":""}, {}, {"name":"Автор два"}
    ]);
    value["audiobook"]["publisher"] = json!("Original publisher");
    value["is_playable"] = json!(false);
    let chapter = mapping::track(&value).unwrap();
    assert_eq!(chapter.artists, ["Original author", "Автор два"]);
    assert_eq!(chapter.album.as_deref(), Some("Original book"));
    assert!(!chapter.capabilities.play);
    assert!(!chapter.capabilities.library_save);
    assert!(!chapter.capabilities.pcm_analysis);
    assert!(!chapter.capabilities.system_media_session);
    let related = mapping::related_collections(&value, &RemoteCatalogKind::Chapter);
    assert_eq!(related[0].subtitle, "Original author, Автор два");
}

#[test]
fn simplified_spoken_items_leave_missing_creators_empty_for_parent_projection() {
    for kind in ["episode", "chapter"] {
        let mut value = spoken_fixture(kind);
        value.as_object_mut().unwrap().remove("show");
        value.as_object_mut().unwrap().remove("audiobook");
        let item = mapping::track(&value).unwrap();
        assert!(item.artists.is_empty());
        assert!(item.album.is_none());
    }
    let mut chapter = spoken_fixture("chapter");
    chapter["audiobook"]["authors"] = json!([{"name":"  "}, {"name":null}]);
    assert!(mapping::track(&chapter).unwrap().artists.is_empty());
    chapter["audiobook"]["publisher"] = json!("Original publisher");
    assert_eq!(
        mapping::track(&chapter).unwrap().artists,
        ["Original publisher"]
    );
    let episode = spoken_fixture("episode");
    assert_eq!(
        mapping::track(&episode).unwrap().artists,
        ["Original publisher"]
    );
    let mut book = collection_fixture("audiobook");
    book["authors"] = json!([{"name":"я".repeat(300)}]);
    assert_eq!(
        mapping::collection_as(&book, RemoteEntityKind::Audiobook)
            .unwrap()
            .subtitle
            .chars()
            .count(),
        256
    );
}
#[test]
fn explicit_context_play_targets_device_and_never_repeats_transfer_on_active_output() {
    let server = Server::new();
    let api = fixture_api(&server);
    let command = RemoteControl::StartContext {
        operation_id: None,
        uri: format!("spotify:album:{}", "a".repeat(22)),
        device_id: "fixture-device".into(),
        item: None,
        position: None,
        query_id: None,
    };
    api.control(&command, Some("fixture-device"))
        .unwrap_or_else(|_| panic!("context"));
    let records = server.requests.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].target.contains("device_id=fixture-device"));
    assert_eq!(
        records[0].body["context_uri"],
        format!("spotify:album:{}", "a".repeat(22))
    );
}
#[test]
fn library_and_playlist_mutations_use_modern_routes_and_receive_real_outcomes() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let id = "a".repeat(22);
    let uris = vec![format!("spotify:track:{}", "4".repeat(22))];
    let edits = vec![
        RemoteEdit::Library {
            uris: uris.clone(),
            save: true,
        },
        RemoteEdit::Library {
            uris: uris.clone(),
            save: false,
        },
        RemoteEdit::CreatePlaylist {
            name: "Original & тест".into(),
            description: "Original description".into(),
            public: false,
            collaborative: None,
        },
        RemoteEdit::PlaylistDetails {
            id: id.clone(),
            name: Some("Renamed".into()),
            description: Some("New description".into()),
            public: Some(false),
            collaborative: None,
        },
        RemoteEdit::PlaylistAdd {
            id: id.clone(),
            uris: uris.clone(),
        },
        RemoteEdit::PlaylistRemove {
            id: id.clone(),
            uris: uris.clone(),
        },
        RemoteEdit::PlaylistMove {
            id: "s".repeat(22),
            query_id: 77,
            from: 0,
            to: 2,
        },
        RemoteEdit::PlaylistReplace { id, uris },
    ];
    for (index, edit) in edits.into_iter().enumerate() {
        if let RemoteEdit::PlaylistMove { id, query_id, .. } = &edit {
            api.catalog(&RemoteQuery {
                query_id: *query_id,
                kind: RemoteCatalogKind::PlaylistItems,
                text: id.clone(),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("move page"));
        }
        validate_edit(&RemoteEditRequest {
            operation_id: index as u64 + 1,
            edit: edit.clone(),
        })
        .unwrap();
        let result = api.edit(&edit).unwrap_or_else(|_| panic!("edit {index}"));
        if index == 2 {
            assert_eq!(result.collection.unwrap().title, "Original playlist");
        }
    }
    let records = server.requests.lock().unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.method == "PUT" && r.target.starts_with("/v1/me/library?uris="))
    );
    assert!(
        records
            .iter()
            .any(|r| r.method == "DELETE" && r.body["items"][0]["uri"].as_str().is_some())
    );
    assert!(records.iter().any(|r| r.body["range_start"] == 0
        && r.body["insert_before"] == 2
        && r.body["snapshot_id"] == "original-version-1"));
    assert!(!records.iter().any(|r| r.target.contains("/tracks")));
}

#[test]
fn playlist_metadata_patch_sends_only_changed_fields_including_explicit_clears() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    for (index, patch) in [
        json!({"name":"Renamed only"}),
        json!({"name":"Семья 👨‍👩‍👧‍👦 · شب\u{200c}ها"}),
        json!({"description":""}),
        json!({"public":false}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut edit = patch.clone();
        edit["kind"] = json!("playlist_details");
        edit["id"] = json!("a".repeat(22));
        let request: RemoteEditRequest =
            serde_json::from_value(json!({"operation_id":index as u64 + 1,"edit":edit})).unwrap();
        validate_edit(&request).unwrap();
        api.edit(&request.edit)
            .unwrap_or_else(|_| panic!("metadata patch"));
        let records = server.requests.lock().unwrap();
        let actual = records.last().unwrap();
        assert_eq!(actual.method, "PUT");
        assert_eq!(
            actual.target.trim_end_matches('?'),
            format!("/v1/playlists/{}", "a".repeat(22))
        );
        assert_eq!(
            actual.body, patch,
            "Unchanged metadata must not overwrite another client's fields"
        );
    }
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}

#[test]
fn playlist_metadata_patch_rejects_empty_and_invalid_changes_before_http() {
    for patch in [
        json!({}),
        json!({"name":"  "}),
        json!({"name":"bad\nname"}),
        json!({"name":"я".repeat(129)}),
        json!({"description":"я".repeat(1025)}),
    ] {
        let mut edit = patch;
        edit["kind"] = json!("playlist_details");
        edit["id"] = json!("a".repeat(22));
        let request: RemoteEditRequest =
            serde_json::from_value(json!({"operation_id":1,"edit":edit})).unwrap();
        assert!(validate_edit(&request).is_err());
    }
}

#[test]
fn collaboration_creation_and_updates_use_single_explicit_requests() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let patches = [
        json!({"kind":"create_playlist","name":"Original collaboration","description":"Original fixture","public":false,"collaborative":true}),
        json!({"kind":"playlist_details","id":"a".repeat(22),"public":false,"collaborative":true}),
        json!({"kind":"playlist_details","id":"a".repeat(22),"collaborative":false}),
        json!({"kind":"playlist_details","id":"a".repeat(22),"public":true,"collaborative":false}),
    ];
    for (index, patch) in patches.into_iter().enumerate() {
        let request: RemoteEditRequest =
            serde_json::from_value(json!({"operation_id":index + 1,"edit":patch})).unwrap();
        validate_edit(&request).unwrap();
        let outcome = api
            .edit(&request.edit)
            .unwrap_or_else(|_| panic!("collaboration update"));
        if index == 0 {
            let created = outcome.collection.unwrap();
            assert_eq!(created.collaborative, Some(true));
            assert_eq!(created.public, Some(false));
        }
        let mut expected = patch;
        expected.as_object_mut().unwrap().remove("kind");
        expected.as_object_mut().unwrap().remove("id");
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), index + 1);
        assert_eq!(records[index].body, expected);
        assert_eq!(
            records[index].method,
            if index == 0 { "POST" } else { "PUT" }
        );
    }
}

#[test]
fn collaboration_rejects_incompatible_or_implicit_publication_without_http() {
    let server = Server::new();
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    for edit in [
        json!({"kind":"create_playlist","name":"Original fixture","description":"","public":true,"collaborative":true}),
        json!({"kind":"playlist_details","id":"a".repeat(22),"public":true,"collaborative":true}),
        json!({"kind":"playlist_details","id":"a".repeat(22),"collaborative":true}),
    ] {
        let request: RemoteEditRequest =
            serde_json::from_value(json!({"operation_id":1,"edit":edit})).unwrap();
        assert!(provider.submit(RemoteAction::Edit { request }).is_err());
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn denied_collaboration_update_is_not_retried() {
    let server = Server::new();
    server.mode.store(403, Ordering::SeqCst);
    let mut api = fixture_api(&server);
    let request: RemoteEditRequest = serde_json::from_value(json!({"operation_id":1,"edit":{"kind":"playlist_details","id":"a".repeat(22),"public":false,"collaborative":true}})).unwrap();
    validate_edit(&request).unwrap();
    assert!(api.edit(&request.edit).is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[test]
fn spotify_profile_membership_uses_only_generic_library_routes() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let uri = "spotify:user:Original.Listener_42-demo";
    let uris = vec![uri.to_owned()];
    let edits = [
        RemoteEdit::CheckLibrary { uris: uris.clone() },
        RemoteEdit::Library {
            uris: uris.clone(),
            save: true,
        },
        RemoteEdit::Library { uris, save: false },
    ];
    for (index, edit) in edits.into_iter().enumerate() {
        validate_edit(&RemoteEditRequest {
            operation_id: index as u64 + 1,
            edit: edit.clone(),
        })
        .unwrap();
        let result = api
            .edit(&edit)
            .unwrap_or_else(|_| panic!("profile membership {index}"));
        if index == 0 {
            assert_eq!(result.saved.len(), 1);
            assert_eq!(result.saved[0].uri, uri);
            assert!(result.saved[0].saved);
        }
    }
    let records = server.records();
    assert_eq!(records.len(), 3);
    for (record, (method, path)) in records.iter().zip([
        ("GET", "/v1/me/library/contains"),
        ("PUT", "/v1/me/library"),
        ("DELETE", "/v1/me/library"),
    ]) {
        let url = reqwest::Url::parse(&format!("http://fixture{}", record.target)).unwrap();
        assert_eq!(record.method, method);
        assert_eq!(url.path(), path);
        assert_eq!(
            url.query_pairs().into_owned().collect::<Vec<_>>(),
            [(String::from("uris"), String::from(uri))]
        );
        assert!(record.authorized);
    }
    assert!(!mapping::valid_uri(uri));
}

#[test]
fn library_membership_rejects_profile_delimiters_and_preserves_chapter_policy() {
    for id in [
        "",
        ".",
        "..",
        "a/b",
        "a:b",
        "a,b",
        "a%2Fb",
        "a&b",
        "a?b",
        "a#b",
        "a b",
        "профиль",
        &"a".repeat(129),
    ] {
        assert!(
            !mapping::valid_library_uri(&format!("spotify:user:{id}")),
            "{id}"
        );
    }
    assert!(mapping::valid_library_uri(
        "spotify:user:short-legacy_id.2~"
    ));
    assert!(mapping::valid_library_uri(&format!(
        "spotify:user:{}",
        "a".repeat(128)
    )));
    let uris = vec![format!("spotify:chapter:{}", "a".repeat(22))];
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::CheckLibrary { uris: uris.clone() }
        })
        .is_ok()
    );
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 2,
            edit: RemoteEdit::Library { uris, save: true }
        })
        .is_err()
    );
}

#[test]
fn catalog_mutations_validate_limits_and_reject_path_injection() {
    let server = Server::new();
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    for kind in [
        RemoteCatalogKind::AlbumTracks,
        RemoteCatalogKind::ShowEpisodes,
        RemoteCatalogKind::ArtistAlbums,
        RemoteCatalogKind::AudiobookChapters,
    ] {
        assert!(
            provider
                .submit(RemoteAction::Query {
                    query: RemoteQuery {
                        query_id: 1,
                        kind,
                        text: "../../me".into(),
                        offset: 0
                    }
                })
                .is_err()
        );
    }
    let uri = format!("spotify:track:{}", "a".repeat(22));
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::Library {
                uris: vec![uri; 41],
                save: true
            }
        })
        .is_err()
    );
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::PlaylistRemove {
                id: "../me".into(),
                uris: vec![]
            }
        })
        .is_err()
    );
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::CreatePlaylist {
                name: "".into(),
                description: "".into(),
                public: false,
                collaborative: None,
            }
        })
        .is_err()
    );
}

fn move_page(api: &mut Api, query_id: u64) -> RemotePage {
    api.catalog(&RemoteQuery {
        query_id,
        kind: RemoteCatalogKind::PlaylistItems,
        text: "s".repeat(22),
        offset: 10,
    })
    .unwrap_or_else(|_| panic!("original sparse playlist page"))
}
fn page_move(query_id: u64) -> RemoteEdit {
    RemoteEdit::PlaylistMove {
        id: "s".repeat(22),
        query_id,
        from: 12,
        to: 10,
    }
}
#[test]
fn playlist_move_targets_the_version_read_before_rows_and_consumes_its_page() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let page = move_page(&mut api, 70);
    assert!(page.position_edits);
    assert_eq!(page.item_positions, [10, 12]);
    assert_eq!(page.items[0].id, page.items[1].id);
    api.catalog(&RemoteQuery {
        query_id: 71,
        kind: RemoteCatalogKind::Profile,
        text: String::new(),
        offset: 0,
    })
    .unwrap_or_else(|_| panic!("detail query"));
    api.edit(&page_move(70))
        .unwrap_or_else(|_| panic!("versioned move"));
    assert_eq!(server.applied_moves.load(Ordering::SeqCst), 1);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    let requests = server.requests.lock().unwrap();
    assert!(requests[0].target.contains("fields=snapshot_id"));
    assert!(requests[1].target.contains("/items?"));
    let put = requests.iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(
        put.body,
        json!({"range_start":12,"insert_before":10,"range_length":1,"snapshot_id":"original-version-1"})
    );
    assert_eq!(requests.iter().filter(|r| r.method == "PUT").count(), 1);
}
#[test]
fn playlist_change_after_browsing_rejects_reorder_without_a_write() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    move_page(&mut api, 70);
    server.playlist_version.store(2, Ordering::SeqCst);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    assert_eq!(server.applied_moves.load(Ordering::SeqCst), 0);
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "PUT")
    );
}
#[test]
fn playlist_change_while_rows_are_loading_cannot_bind_new_rows_to_an_old_version() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    server.change_on_items.store(true, Ordering::SeqCst);
    move_page(&mut api, 70);
    assert_eq!(server.playlist_version.load(Ordering::SeqCst), 2);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "PUT")
    );
}
#[test]
fn playlist_race_after_preflight_is_sent_with_its_snapshot_and_never_retried() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    move_page(&mut api, 70);
    server.change_on_move.store(true, Ordering::SeqCst);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    assert_eq!(server.applied_moves.load(Ordering::SeqCst), 0);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PUT")
            .count(),
        1
    );
}
#[test]
fn playlist_move_rejects_wrong_page_playlist_generation_and_omitted_position_before_io() {
    for case in 0..4 {
        let server = Server::new();
        let mut api = fixture_api(&server);
        move_page(&mut api, 70);
        let mut edit = page_move(70);
        if let RemoteEdit::PlaylistMove {
            id, query_id, from, ..
        } = &mut edit
        {
            match case {
                0 => *query_id = 69,
                1 => *id = "x".repeat(22),
                2 => api.generation += 1,
                _ => *from = 11,
            }
        }
        let before = server.requests.lock().unwrap().len();
        assert_eq!(
            api.edit(&edit).err().unwrap().error.code,
            ErrorCode::CatalogChanged
        );
        assert_eq!(server.requests.lock().unwrap().len(), before);
    }
}
#[test]
fn missing_playlist_snapshot_preserves_browsing_without_positional_edit_permission() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    server.playlist_version.store(0, Ordering::SeqCst);
    let page = move_page(&mut api, 70);
    assert!(!page.position_edits && page.items.len() == 2);
    assert_eq!(
        api.edit(&page_move(70)).err().unwrap().error.code,
        ErrorCode::CatalogChanged
    );
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "PUT")
    );
}
#[test]
fn another_catalog_page_or_uncertain_playlist_write_invalidates_position_context() {
    for edit_first in [false, true] {
        let server = Server::new();
        let mut api = fixture_api(&server);
        move_page(&mut api, 70);
        if edit_first {
            server.mode.store(294, Ordering::SeqCst);
            assert!(
                api.edit(&RemoteEdit::PlaylistAdd {
                    id: "s".repeat(22),
                    uris: vec![format!("spotify:track:{}", "4".repeat(22))]
                })
                .is_err()
            );
        } else {
            api.catalog(&RemoteQuery {
                query_id: 71,
                kind: RemoteCatalogKind::Search,
                text: "Original".into(),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("new search"));
        }
        let before = server.requests.lock().unwrap().len();
        assert_eq!(
            api.edit(&page_move(70)).err().unwrap().error.code,
            ErrorCode::CatalogChanged
        );
        assert_eq!(server.requests.lock().unwrap().len(), before);
    }
}

#[test]
fn library_status_and_cover_upload_are_bounded_and_use_real_response_shapes() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let uri = format!("spotify:track:{}", "a".repeat(22));
    let result = api
        .edit(&RemoteEdit::CheckLibrary {
            uris: vec![uri.clone()],
        })
        .unwrap_or_else(|_| panic!("library check"));
    assert_eq!(result.saved.len(), 1);
    assert_eq!(result.saved[0].uri, uri);
    assert!(result.saved[0].saved);
    let edit = RemoteEdit::PlaylistCover {
        id: "a".repeat(22),
        jpeg_base64: "/9j/AA==".into(),
    };
    validate_edit(&RemoteEditRequest {
        operation_id: 1,
        edit: edit.clone(),
    })
    .unwrap();
    api.edit(&edit).unwrap_or_else(|_| panic!("cover upload"));
    let records = server.requests.lock().unwrap();
    let cover = records
        .iter()
        .find(|r| r.target.contains("/images"))
        .unwrap();
    assert_eq!(cover.method, "PUT");
    assert_eq!(cover.content_length, Some(8));
    assert!(cover.body.is_null());
    drop(records);
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::PlaylistCover {
                id: "a".repeat(22),
                jpeg_base64: "a".repeat(256 * 1024 + 4)
            }
        })
        .is_err()
    );
    assert!(
        validate_edit(&RemoteEditRequest {
            operation_id: 1,
            edit: RemoteEdit::PlaylistCover {
                id: "a".repeat(22),
                jpeg_base64: "../../a!".into()
            }
        })
        .is_err()
    );
}
#[test]
fn unknown_playlist_visibility_is_not_overwritten_by_renaming() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    api.edit(&RemoteEdit::PlaylistDetails {
        id: "a".repeat(22),
        name: Some("Renamed".into()),
        description: Some("Original".into()),
        public: None,
        collaborative: None,
    })
    .unwrap_or_else(|_| panic!("rename"));
    let records = server.requests.lock().unwrap();
    assert!(records[0].body.get("public").is_none());
}
#[test]
fn asynchronous_library_operation_has_its_own_completion_and_disconnect_generation() {
    let server = Server::new();
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    let edit = RemoteEdit::Library {
        uris: vec![format!("spotify:track:{}", "a".repeat(22))],
        save: true,
    };
    provider
        .submit(RemoteAction::Edit {
            request: RemoteEditRequest {
                operation_id: 41,
                edit: edit.clone(),
            },
        })
        .unwrap();
    assert!(
        provider
            .submit(RemoteAction::Edit {
                request: RemoteEditRequest {
                    operation_id: 42,
                    edit: edit.clone()
                }
            })
            .is_err()
    );
    let result = wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Edited(result) if result.operation_id==41),
    );
    if let RemoteUpdate::Edited(result) = result {
        assert!(result.error.is_none());
    } else {
        panic!("edit result");
    }
    server.delay.store(150, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Edit {
            request: RemoteEditRequest {
                operation_id: 43,
                edit,
            },
        })
        .unwrap();
    provider.submit(RemoteAction::Disconnect).unwrap();
    thread::sleep(Duration::from_millis(200));
    while let Some(update) = provider.poll() {
        assert!(!matches!(update, RemoteUpdate::Edited(_)));
    }
}

#[test]
fn context_confirmation_waits_for_the_requested_album_and_device() {
    let uri = format!("spotify:album:{}", "a".repeat(22));
    let confirmation = ControlConfirmation {
        control: RemoteControl::StartContext {
            operation_id: None,
            uri: uri.clone(),
            device_id: "fixture-device".into(),
            item: None,
            position: None,
            query_id: None,
        },
        previous: None,
        until: Instant::now() + Duration::from_secs(8),
        attempts: 0,
    };
    let mut value = frame();
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["context"] = json!({"uri":uri});
    assert!(confirmation.matches(Some(&mapping::playback(&value))));
    value["device"]["id"] = json!("other-device");
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    assert!(!chapter.capabilities.library_save);
    assert!(
        mapping::track(&track('a'))
            .unwrap()
            .capabilities
            .library_save
    );
}

#[test]
fn detail_admission_is_bounded_and_does_not_replace_catalog_intent() {
    let server = Server::new();
    let mut provider = server.provider();
    for query_id in 1..=8 {
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id,
                    kind: RemoteCatalogKind::Album,
                    text: "a".repeat(22),
                    offset: 0,
                },
            })
            .unwrap();
    }
    assert!(
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id: 9,
                    kind: RemoteCatalogKind::Profile,
                    text: String::new(),
                    offset: 0
                }
            })
            .is_err()
    );
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 10,
                kind: RemoteCatalogKind::Search,
                text: "original fixture".into(),
                offset: 0,
            },
        })
        .unwrap();
    {
        let mailbox = provider.shared.0.lock().unwrap();
        assert_eq!(mailbox.details.len(), 8);
        assert_eq!(mailbox.detail_credits.len(), 8);
        assert_eq!(mailbox.query.as_ref().unwrap().query_id, 10);
        assert!(server.records().is_empty());
    }
    provider.submit(RemoteAction::Disconnect).unwrap();
    let mailbox = provider.shared.0.lock().unwrap();
    assert!(
        mailbox.details.is_empty()
            && mailbox.detail_credits.is_empty()
            && mailbox.detail_pages.is_empty()
            && mailbox.query.is_none()
    );
}
#[test]
fn simultaneous_catalog_and_profile_have_independent_results() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    for (query_id, kind) in [
        (100, RemoteCatalogKind::SavedTracks),
        (101, RemoteCatalogKind::Profile),
        (102, RemoteCatalogKind::HomeRadio),
    ] {
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id,
                    kind,
                    text: String::new(),
                    offset: 0,
                },
            })
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = std::collections::BTreeMap::new();
    while seen.len() < 3 && Instant::now() < deadline {
        if let Some(RemoteUpdate::Page(page)) = provider.poll() {
            seen.insert(page.query_id, page);
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(seen.len(), 3);
    assert!(!seen[&100].detail);
    assert!(seen[&101].detail);
    assert_eq!(seen[&101].collections[0].title, "Original fixture listener");
    assert_eq!(seen[&101].collections[0].id, "original-test-user");
    assert_eq!(
        seen[&101].collections[0].account_id.as_deref(),
        Some("original-stable-account")
    );
    assert!(
        seen[&101].collections[0]
            .url
            .ends_with("/user/original-test-user")
    );
    assert!(seen[&102].detail);
    assert_eq!(seen[&102].collections[0].kind, RemoteEntityKind::Playlist);
    assert!(seen[&102].collections[0].account_id.is_none());
    assert!(provider.shared.0.lock().unwrap().detail_credits.is_empty());
}
#[test]
fn malformed_or_missing_immutable_profile_ids_leave_a_legacy_profile() {
    for account_id in [
        Value::Null,
        json!(12),
        json!(""),
        json!("bad\nkey"),
        json!("bad key"),
        json!("a".repeat(257)),
    ] {
        let server =
            Server::with_profile(json!({"id":"original-test-user","account_id":account_id}));
        let page = fixture_api(&server)
            .catalog(&RemoteQuery {
                query_id: 1,
                kind: RemoteCatalogKind::Profile,
                text: String::new(),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("profile query"));
        let profile = &page.collections[0];
        assert_eq!(profile.id, "original-test-user");
        assert!(profile.account_id.is_none());
        assert!(
            serde_json::to_value(profile)
                .unwrap()
                .get("account_id")
                .is_none()
        );
    }
}
#[test]
fn home_reads_fixed_first_pages_without_invalidating_playlist_positions() {
    use RemoteCatalogKind::*;
    let server = Server::new();
    let mut api = fixture_api(&server);
    api.catalog(&RemoteQuery {
        query_id: 1,
        kind: PlaylistItems,
        text: "p".repeat(22),
        offset: 0,
    })
    .unwrap_or_else(|_| panic!("positioned playlist"));
    assert!(api.playlist_page.is_some());
    let first_home_request = server.records().len();
    let artist_id = "r".repeat(22);
    for (index, (kind, text)) in [
        (HomeRecent, ""),
        (HomeTopTracks, ""),
        (HomeTopArtists, ""),
        (HomePlaylists, ""),
        (HomeRadio, ""),
        (HomeArtistAlbums, artist_id.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let page = api
            .catalog(&RemoteQuery {
                query_id: index as u64 + 2,
                kind,
                text: text.into(),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("home shelf"));
        assert!(page.detail);
        assert_eq!(page.items.len() + page.collections.len(), 1);
        assert!(api.playlist_page.is_some());
    }
    let records = server.records();
    let requests = &records[first_home_request..];
    assert_eq!(requests.len(), 6);
    let artist_albums = format!("/v1/artists/{artist_id}/albums");
    for (request, endpoint) in requests.iter().zip([
        "/v1/me/player/recently-played",
        "/v1/me/top/tracks",
        "/v1/me/top/artists",
        "/v1/me/playlists",
        "/v1/search",
        artist_albums.as_str(),
    ]) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.target.split('?').next(), Some(endpoint));
        assert!(request.target.contains("limit=10"));
    }
    assert!(
        requests[4].target.contains("q=radio+mix") && requests[4].target.contains("type=playlist")
    );
    let mut provider = server.provider();
    for (kind, offset, text) in [
        (HomeRadio, 1, ""),
        (HomeRadio, 0, "unbounded input"),
        (HomeArtistAlbums, 0, ""),
        (HomeArtistAlbums, 0, "../untrusted-path"),
        (HomeArtistAlbums, 1, artist_id.as_str()),
    ] {
        assert!(
            provider
                .submit(RemoteAction::Query {
                    query: RemoteQuery {
                        query_id: 12,
                        kind,
                        text: text.into(),
                        offset,
                    },
                })
                .is_err()
        );
    }
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 12,
                kind: HomeArtistAlbums,
                text: artist_id,
                offset: 0,
            },
        })
        .unwrap();
}
#[test]
fn artist_chart_and_catalog_selection_preserve_source_order_and_identity() {
    use RemoteCatalogKind::*;
    let server = Server::new();
    let mut api = fixture_api(&server);
    let id = "r".repeat(22);
    let query = |kind, offset| RemoteQuery {
        query_id: 1,
        kind,
        text: id.clone(),
        offset,
    };
    let chart = api.catalog(&query(ArtistTopTracks, 0)).ok().unwrap();
    assert!(chart.detail && chart.issue.is_none());
    assert_eq!(
        chart.items[0].id.as_str(),
        format!("spotify:track:{}", "4".repeat(22))
    );
    assert_eq!(
        chart.items[1].id.as_str(),
        format!("spotify:track:{}", "6".repeat(22))
    );
    server.mode.store(403, Ordering::SeqCst);
    let denied = api.catalog(&query(ArtistTopTracks, 0)).err().unwrap();
    assert_eq!(denied.error.code, ErrorCode::UnsupportedCapability);
    server.mode.store(200, Ordering::SeqCst);

    let selected = api.catalog(&query(ArtistSelectedTracks, 0)).ok().unwrap();
    assert!(selected.detail && selected.issue.is_none());
    assert_eq!(selected.item_positions, vec![1, 4]);
    assert_eq!(selected.items[0].id, chart.items[1].id);
    assert_eq!(selected.items[1].id, chart.items[0].id);

    let releases = api.catalog(&query(ArtistAlbums, 10)).ok().unwrap();
    assert!(releases.detail);
    assert_eq!(releases.offset, 10);
    assert_eq!(releases.collections[0].subtitle, "Album · 2024");
    let records = server.records();
    assert!(
        records
            .iter()
            .any(|r| r.target.trim_end_matches('?') == format!("/v1/artists/{id}/top-tracks"))
    );
    assert!(records.iter().any(|r| r.target.starts_with("/v1/search?")
        && r.target.contains("artist%3A%22Original+artist%22")));

    let mut provider = server.provider();
    assert!(
        provider
            .submit(RemoteAction::Query {
                query: query(ArtistTopTracks, 10)
            })
            .is_err()
    );
    assert!(
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    text: "../untrusted".into(),
                    ..query(ArtistSelectedTracks, 0)
                },
            })
            .is_err()
    );
}
#[test]
fn playlist_positions_keep_holes_and_duplicate_occurrences() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let page = api
        .catalog(&RemoteQuery {
            query_id: 22,
            kind: RemoteCatalogKind::PlaylistItems,
            text: "s".repeat(22),
            offset: 10,
        })
        .unwrap_or_else(|_| panic!("playlist positions catalog"));
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.item_positions, vec![10, 12]);
    assert_eq!(page.items[0].id, page.items[1].id);
    assert!(page.issue.is_some());
}
#[test]
fn item_details_expose_related_collections_without_followup_requests() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    for (kind, expected) in [
        (
            RemoteCatalogKind::Track,
            vec![RemoteEntityKind::Album, RemoteEntityKind::Artist],
        ),
        (RemoteCatalogKind::Episode, vec![RemoteEntityKind::Show]),
        (
            RemoteCatalogKind::Chapter,
            vec![RemoteEntityKind::Audiobook],
        ),
    ] {
        let page = api
            .catalog(&RemoteQuery {
                query_id: 44,
                kind,
                text: "e".repeat(22),
                offset: 0,
            })
            .unwrap_or_else(|_| panic!("related catalog"));
        assert_eq!(page.items.len(), 1);
        assert!(page.collections.is_empty());
        assert_eq!(
            page.related_collections
                .iter()
                .map(|c| c.kind)
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(server.records().len(), 3);
}
#[test]
fn relationships_require_real_ids_and_are_bounded_and_deduplicated() {
    let mut value = track('4');
    value["artists"] = json!(
        (0..40)
            .map(|n| json!({"id":format!("{n:022}"),"name":"Original bounded author"}))
            .collect::<Vec<_>>()
    );
    let related = mapping::related_collections(&value, &RemoteCatalogKind::Track);
    assert_eq!(related.len(), 17);
    value["artists"] = json!([value["artists"][0].clone(),value["artists"][0].clone(),{"id":"../bad","name":"Invalid creator"}]);
    assert_eq!(
        mapping::related_collections(&value, &RemoteCatalogKind::Track).len(),
        2
    );
    value["album"]["id"] = Value::Null;
    value["artists"] = json!([{"name":"No identifier"}]);
    assert!(mapping::related_collections(&value, &RemoteCatalogKind::Track).is_empty());
    assert!(mapping::related_collections(&track('4'), &RemoteCatalogKind::Search).is_empty());
}
#[test]
fn playlist_metadata_requests_do_not_download_the_item_list() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    api.catalog(&RemoteQuery {
        query_id: 45,
        kind: RemoteCatalogKind::PlaylistInfo,
        text: "a".repeat(22),
        offset: 0,
    })
    .unwrap_or_else(|_| panic!("playlist metadata"));
    let records = server.records();
    let url = reqwest::Url::parse(&format!("http://fixture{}", records[0].target)).unwrap();
    let fields = url
        .query_pairs()
        .find(|(name, _)| name == "fields")
        .unwrap()
        .1
        .into_owned();
    assert!(fields.contains("owner(id,display_name)") && fields.contains("collaborative"));
    assert!(!fields.contains("items") && !fields.contains("tracks"));
    assert_eq!(records.len(), 1);
}
#[test]
fn resume_falls_back_to_the_exact_item_for_contexts_without_offset_support() {
    for context_uri in [None, Some("spotify:artist:".to_owned() + &"a".repeat(22))] {
        let server = Server::new();
        let api = fixture_api(&server);
        let item = mapping::track(&track('4')).unwrap();
        api.control(
            &RemoteControl::ResumeOnDevice {
                operation_id: None,
                item: item.clone(),
                device_id: "new-device".into(),
                position_ms: 3456,
                context_uri,
            },
            Some("old-device"),
        )
        .unwrap_or_else(|_| panic!("resume item"));
        let records = server.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target, "/v1/me/player/play?device_id=new-device");
        assert_eq!(records[0].body["uris"][0], item.id.as_str());
        assert_eq!(records[0].body["position_ms"], 3456);
        assert!(records[0].body.get("context_uri").is_none());
    }
}
#[test]
fn rejected_targeted_resume_never_transfers_the_previous_session() {
    let server = Server::new();
    server.mode.store(292, Ordering::SeqCst);
    let api = fixture_api(&server);
    let error = api
        .control(
            &RemoteControl::ResumeOnDevice {
                operation_id: Some(5),
                item: mapping::track(&track('4')).unwrap(),
                device_id: "unavailable-output".into(),
                position_ms: 3456,
                context_uri: None,
            },
            Some("phone"),
        )
        .expect_err("Resume should be rejected");
    assert_eq!(error.status, 404);
    let records = server.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].target,
        "/v1/me/player/play?device_id=unavailable-output"
    );
}
#[test]
fn invalid_resume_never_transfers_or_plays_and_pause_cancels_a_queued_resume() {
    let server = Server::new();
    let api = fixture_api(&server);
    let item = mapping::track(&track('4')).unwrap();
    for (device_id, position_ms, context_uri) in [
        ("owned-device", 9000, None),
        ("bad device", 1000, None),
        (
            "owned-device",
            1000,
            Some("https://example.invalid/not-a-context".into()),
        ),
    ] {
        assert!(
            api.control(
                &RemoteControl::ResumeOnDevice {
                    operation_id: None,
                    item: item.clone(),
                    device_id: device_id.into(),
                    position_ms,
                    context_uri
                },
                None
            )
            .is_err()
        );
    }
    assert!(server.records().is_empty());
    assert!(supersedes(
        &RemoteControl::Pause,
        &RemoteControl::ResumeOnDevice {
            operation_id: None,
            item,
            device_id: "owned-device".into(),
            position_ms: 1000,
            context_uri: None
        }
    ));
}
#[test]
fn cancelling_resume_pauses_only_its_device_and_accepts_an_already_gone_output() {
    let server = Server::new();
    let api = fixture_api(&server);
    let pause = RemoteControl::PauseOnDevice {
        device_id: "owned-output".into(),
    };
    api.control(&pause, Some("phone"))
        .unwrap_or_else(|_| panic!("directed pause"));
    server.mode.store(292, Ordering::SeqCst);
    api.control(&pause, Some("phone"))
        .unwrap_or_else(|_| panic!("already inactive output"));
    let records = server.records();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|r| r.target.contains("device_id=owned-output") && !r.target.contains("phone"))
    );
    assert!(supersedes(
        &pause,
        &RemoteControl::ResumeOnDevice {
            operation_id: None,
            item: mapping::track(&track('4')).unwrap(),
            device_id: "owned-output".into(),
            position_ms: 1000,
            context_uri: None
        }
    ));
    assert!(!supersedes(
        &pause,
        &RemoteControl::ResumeOnDevice {
            operation_id: None,
            item: mapping::track(&track('4')).unwrap(),
            device_id: "phone".into(),
            position_ms: 1000,
            context_uri: None
        }
    ));
}

#[test]
fn correlated_start_rejections_are_local_to_the_request_and_survive_follow_up_reads() {
    for mode in 290..=292 {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        server.mode.store(mode, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::StartOnDevice {
                    item: mapping::track(&track('8')).unwrap(),
                    device_id: "fixture-device".into(),
                    operation_id: Some(41),
                },
            })
            .unwrap();
        let result = wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.control_result.as_ref().is_some_and(|r| r.operation_id == 41 && r.error.is_some())),
        );
        let RemoteUpdate::State(state) = result else {
            unreachable!()
        };
        assert_eq!(state.connection, ProviderConnectionState::Connected);
        assert!(
            state.issue.is_none(),
            "A previous start error must not become a newer request's global playback issue"
        );
        assert_eq!(
            state
                .playback
                .as_ref()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .id
                .as_str(),
            format!("spotify:track:{}", "1".repeat(22))
        );
        provider.submit(RemoteAction::Refresh).unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.observation_id != state.observation_id && s.control_result.as_ref().is_some_and(|r| r.operation_id == 41 && r.error.is_some()) && s.issue.is_none()),
        );
        assert_eq!(
            server
                .records()
                .iter()
                .filter(|r| r.method == "PUT" && r.target.starts_with("/v1/me/player/play"))
                .count(),
            1
        );
        server.mode.store(200, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Credential {
                access_token: "fixture-new-generation".into(),
            })
            .unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Connected && s.control_result.is_none()),
        );
    }
}
#[test]
fn correlated_success_reports_dispatch_without_inventing_actual_playback_or_sending_its_id_to_spotify()
 {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartOnDevice {
                item: mapping::track(&track('8')).unwrap(),
                device_id: "fixture-device".into(),
                operation_id: Some(42),
            },
        })
        .unwrap();
    let result = wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.control_result.as_ref().is_some_and(|r| r.operation_id == 42 && r.error.is_none())),
    );
    let RemoteUpdate::State(state) = result else {
        unreachable!()
    };
    assert_eq!(
        state
            .playback
            .as_ref()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .id
            .as_str(),
        format!("spotify:track:{}", "1".repeat(22))
    );
    let request = server
        .records()
        .into_iter()
        .find(|r| r.method == "PUT" && r.target.starts_with("/v1/me/player/play"))
        .unwrap();
    assert_eq!(
        request.body,
        json!({"uris":[format!("spotify:track:{}", "8".repeat(22))]})
    );
}
#[test]
fn a_failed_observation_does_not_relabel_a_successful_dispatch_as_a_control_rejection() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.mode.store(293, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartOnDevice {
                item: mapping::track(&track('8')).unwrap(),
                device_id: "fixture-device".into(),
                operation_id: Some(43),
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Unavailable && s.issue.is_some() && s.control_result.as_ref().is_some_and(|r| r.operation_id == 43 && r.error.is_none())),
    );
}
#[test]
fn legacy_start_json_remains_valid_and_zero_operation_identity_is_rejected_before_io() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let item = mapping::track(&track('8')).unwrap();
    let legacy: RemoteControl = serde_json::from_value(
        json!({"kind":"start_on_device","item":item,"device_id":"fixture-device"}),
    )
    .unwrap();
    assert!(legacy.operation_id().is_none());
    let invalid = RemoteControl::StartOnDevice {
        item,
        device_id: "fixture-device".into(),
        operation_id: Some(0),
    };
    assert!(
        provider
            .submit(RemoteAction::Control {
                control: invalid.clone()
            })
            .is_err()
    );
    assert!(
        fixture_api(&server)
            .control(&invalid, Some("fixture-device"))
            .is_err()
    );
    assert!(!server.records().iter().any(|r| r.method == "PUT"));
}

mod recovery;
