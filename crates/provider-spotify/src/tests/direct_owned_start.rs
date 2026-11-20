use super::*;

pub(super) const REJECT_FIRST_PLAY: u16 = 295;
const DEVICE: &str = "fixture-device";
const OPERATION: u64 = 9;

fn album() -> RemoteControl {
    RemoteControl::StartContext {
        uri: format!("spotify:album:{}", "a".repeat(22)),
        device_id: DEVICE.into(),
        operation_id: Some(OPERATION),
        item: None,
        position: None,
        query_id: None,
    }
}

fn song() -> RemoteControl {
    RemoteControl::StartOnDevice {
        item: mapping::track(&track('6')).unwrap(),
        device_id: DEVICE.into(),
        operation_id: Some(OPERATION),
    }
}

fn active_context_clock() -> RemoteOutputClock {
    let mut clock = output_clock(1);
    clock.media_id = Some(MediaId::parse(format!("spotify:track:{}", "6".repeat(22))).unwrap());
    clock.active = true;
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.control_uptime_ms = Some(100);
    clock.queue_epoch = Some(7);
    clock.metadata = Some(RemoteOutputMetadata {
        title: "Original active launch selection".into(),
        artists: vec!["Auvra original fixture".into()],
        album: None,
        duration_ms: 8_000,
        artwork: None,
        context_uri: Some(format!("spotify:album:{}", "o".repeat(22))),
    });
    // Bootstrap's device_changed event removes actions. HTTP-order admission
    // must not mistake this for a grant of any native playback capability.
    clock.actions = None;
    clock
}

fn active_context_base() -> RemoteState {
    let mut base = output_base(1);
    base.devices
        .push(base.playback.take().unwrap().device.unwrap());
    base
}

fn active_direct_api(
    server: &Server,
    selection: &RemoteControl,
) -> (Api, Shared, output_clock::OutputProjection) {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.ingest(active_context_base(), now);
    output.observe(Some(active_context_clock()), now).unwrap();
    let start = output.remember_http_selection(selection, now).unwrap();
    assert!(start.active_context_origin.is_some());
    let projected = output.take(now).unwrap();
    assert!(projected.playback.is_none());
    assert!(!projected.owned_output_observed && !projected.owned_output_control);
    assert!(!output.has_owned_binding() && output.control_clock(now).is_none());
    assert!(output.owned_capture_witness(now).is_none());
    let (api, shared) = direct_api(server, Some(now));
    shared.0.lock().unwrap().direct_owned_start = Some(start);
    (api, shared, output)
}

fn direct_api(server: &Server, observed_at: Option<Instant>) -> (Api, Shared) {
    let shared = Arc::new((
        Mutex::new(Mailbox {
            direct_owned_start: Some(DirectOwnedStart {
                operation: OPERATION,
                generation: 1,
                device: DEVICE.into(),
                observed_at,
                active_context_origin: None,
            }),
            ..Mailbox::default()
        }),
        Condvar::new(),
    ));
    let mut api = Api::new(server.base.clone(), shared.clone()).unwrap();
    api.token = Some("fixture-bearer".into());
    (api, shared)
}

fn writes(server: &Server) -> Vec<Request> {
    server
        .records()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect()
}

#[test]
fn owned_cold_song_and_album_target_play_without_transferring_previous_audio() {
    for (selection, body) in [
        (
            song(),
            json!({"uris":[format!("spotify:track:{}", "6".repeat(22))]}),
        ),
        (
            album(),
            json!({"context_uri":format!("spotify:album:{}", "a".repeat(22))}),
        ),
    ] {
        let server = Server::new();
        let (api, _) = direct_api(&server, Some(Instant::now()));
        assert!(
            api.control_with_owned_start(&selection, Some("other-device"), true, false)
                .is_ok()
        );
        let writes = writes(&server);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].target,
            "/v1/me/player/play?device_id=fixture-device"
        );
        assert_eq!(writes[0].body, body);
    }
}

#[test]
fn owned_cold_album_404_retries_exact_body_after_one_transfer() {
    let server = Server::new();
    server.mode.store(REJECT_FIRST_PLAY, Ordering::SeqCst);
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = output_clock(1);
    clock.state = PlaybackState::Paused;
    clock.active = false;
    clock.rate = 0.0;
    clock.control_uptime_ms = Some(100);
    provider
        .submit(RemoteAction::OutputClock { clock: Some(clock) })
        .unwrap();
    provider
        .submit(RemoteAction::Control { control: album() })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == OPERATION && result.error.is_none())),
    );
    let writes = writes(&server);
    assert_eq!(writes.len(), 3);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/play?device_id=fixture-device"
    );
    assert_eq!(writes[1].target.trim_end_matches('?'), "/v1/me/player");
    assert_eq!(writes[1].body, json!({"device_ids":[DEVICE],"play":false}));
    assert_eq!(writes[2].target, writes[0].target);
    assert_eq!(writes[2].body, writes[0].body);
}

#[test]
fn owned_cold_play_uncertain_or_forbidden_writes_never_transfer_or_replay() {
    for (mode, status) in [(294, 503), (291, 403)] {
        let server = Server::new();
        server.mode.store(mode, Ordering::SeqCst);
        let (api, _) = direct_api(&server, Some(Instant::now()));
        let result = api.control_with_owned_start(&album(), None, true, false);
        assert_eq!(result.err().unwrap().status, status);
        assert_eq!(writes(&server).len(), 1);
        assert_eq!(
            writes(&server)[0].target,
            "/v1/me/player/play?device_id=fixture-device"
        );
    }
}

#[test]
fn owned_cold_play_checks_freshness_before_writing() {
    let server = Server::new();
    let (api, _) = direct_api(&server, Some(Instant::now() - Duration::from_secs(2)));
    let result = api.control_with_owned_start(&song(), None, true, false);
    assert_eq!(result.err().unwrap().status, 499);
    assert!(writes(&server).is_empty());
}

#[test]
fn owned_cold_play_does_not_transfer_after_pause_arrives_during_404() {
    let server = Server::new();
    server.mode.store(REJECT_FIRST_PLAY, Ordering::SeqCst);
    server.hold_playback.store(true, Ordering::SeqCst);
    let (api, shared) = direct_api(&server, Some(Instant::now()));
    let request = thread::spawn(move || api.control_with_owned_start(&song(), None, true, false));
    let deadline = Instant::now() + Duration::from_secs(2);
    while writes(&server).is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writes(&server).len(), 1);
    shared
        .0
        .lock()
        .unwrap()
        .controls
        .push_back(RemoteControl::Pause.into());
    server.hold_playback.store(false, Ordering::SeqCst);
    assert_eq!(request.join().unwrap().err().unwrap().status, 499);
    assert_eq!(writes(&server).len(), 1);
}

#[test]
fn active_unbound_album_and_playlist_use_one_addressed_play_without_new_authority() {
    for kind in ["album", "playlist"] {
        let selection = RemoteControl::StartContext {
            uri: format!("spotify:{kind}:{}", "a".repeat(22)),
            device_id: DEVICE.into(),
            operation_id: Some(OPERATION),
            item: None,
            position: None,
            query_id: None,
        };
        let server = Server::new();
        let (api, _, output) = active_direct_api(&server, &selection);
        assert!(
            api.control_with_owned_start(&selection, None, true, false)
                .is_ok()
        );
        let writes = writes(&server);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].target,
            "/v1/me/player/play?device_id=fixture-device"
        );
        assert_eq!(
            writes[0].body,
            json!({"context_uri":format!("spotify:{kind}:{}", "a".repeat(22))})
        );
        assert!(!output.has_owned_binding());
        assert!(output.owned_capture_witness(Instant::now()).is_none());
    }
}

#[test]
fn active_unbound_context_retries_identical_target_only_after_404() {
    let server = Server::new();
    server.mode.store(REJECT_FIRST_PLAY, Ordering::SeqCst);
    let (api, _, _) = active_direct_api(&server, &album());
    assert!(
        api.control_with_owned_start(&album(), None, true, false)
            .is_ok()
    );
    let recorded = writes(&server);
    assert_eq!(recorded.len(), 3);
    assert_eq!(
        recorded[0].target,
        "/v1/me/player/play?device_id=fixture-device"
    );
    assert_eq!(recorded[1].target.trim_end_matches('?'), "/v1/me/player");
    assert_eq!(
        recorded[1].body,
        json!({"device_ids":[DEVICE],"play":false})
    );
    assert_eq!(recorded[2].target, recorded[0].target);
    assert_eq!(recorded[2].body, recorded[0].body);
    for (mode, status) in [(294, 503), (291, 403)] {
        let server = Server::new();
        server.mode.store(mode, Ordering::SeqCst);
        let (api, _, _) = active_direct_api(&server, &album());
        assert_eq!(
            api.control_with_owned_start(&album(), None, true, false)
                .err()
                .unwrap()
                .status,
            status
        );
        assert_eq!(writes(&server).len(), 1);
    }
}

#[test]
fn active_context_direct_admission_does_not_cover_foreign_partial_stale_or_other_controls() {
    for kind in [
        "foreign frame",
        "foreign discovery",
        "duplicate",
        "restricted",
        "unknown",
        "authentication",
        "unavailable",
        "stale",
        "no uptime",
        "no metadata",
        "no media",
        "no epoch",
        "loading",
        "song",
        "row",
        "resume",
        "bound",
    ] {
        let now = Instant::now();
        let mut output = output_clock::OutputProjection::default();
        let mut base = active_context_base();
        match kind {
            "foreign frame" => {
                base.playback = output_base(2).playback;
                base.playback.as_mut().unwrap().device.as_mut().unwrap().id =
                    "foreign-device".into();
            }
            "foreign discovery" => {
                let mut foreign = base.devices[0].clone();
                foreign.id = "foreign-device".into();
                foreign.is_active = true;
                base.devices.push(foreign);
            }
            "duplicate" => base.devices.push(base.devices[0].clone()),
            "restricted" => base.devices[0].is_restricted = true,
            "unknown" => base.devices.clear(),
            "authentication" => base.authentication_required = true,
            "unavailable" => base.connection = ProviderConnectionState::Unavailable,
            "bound" => {
                base.playback = output_base(2).playback;
            }
            _ => {}
        }
        output.ingest(base, now);
        let mut clock = active_context_clock();
        match kind {
            "no uptime" => clock.control_uptime_ms = None,
            "no metadata" => clock.metadata = None,
            "no media" => {
                clock.media_id = None;
                clock.state = PlaybackState::Idle;
                clock.metadata = None;
            }
            "no epoch" => clock.queue_epoch = None,
            "loading" => clock.state = PlaybackState::Loading,
            "bound" => {
                clock.media_id = output_clock(1).media_id;
            }
            _ => {}
        }
        output
            .observe(
                Some(clock),
                if kind == "stale" {
                    now - Duration::from_secs(2)
                } else {
                    now
                },
            )
            .unwrap();
        if kind == "bound" {
            assert!(output.take(now).unwrap().owned_output_observed);
        }
        let selection = match kind {
            "song" => song(),
            "row" => RemoteControl::StartContext {
                uri: format!("spotify:album:{}", "a".repeat(22)),
                device_id: DEVICE.into(),
                operation_id: Some(OPERATION),
                item: Some(mapping::track(&track('6')).unwrap()),
                position: Some(0),
                query_id: Some(1),
            },
            "resume" => RemoteControl::ResumeOnDevice {
                item: mapping::track(&track('6')).unwrap(),
                device_id: DEVICE.into(),
                position_ms: 3_000,
                context_uri: Some(format!("spotify:album:{}", "a".repeat(22))),
                operation_id: Some(OPERATION),
            },
            _ => album(),
        };
        assert!(
            output.remember_http_selection(&selection, now).is_none(),
            "{kind}"
        );
    }
}

#[test]
fn active_context_404_cannot_transfer_after_scope_or_intent_changes() {
    for kind in [
        "nil",
        "runtime",
        "device",
        "media",
        "context",
        "epoch",
        "backward uptime",
        "inactive",
        "foreign",
        "restriction",
        "pause",
        "selection",
        "transfer",
        "credentials",
        "auth",
        "pending foreign discovery",
        "pending restricted discovery",
        "pending merged foreign",
        "auxiliary failure",
    ] {
        let server = Server::new();
        server.mode.store(REJECT_FIRST_PLAY, Ordering::SeqCst);
        server.hold_playback.store(true, Ordering::SeqCst);
        let (api, shared, mut output) = active_direct_api(&server, &album());
        let request =
            thread::spawn(move || api.control_with_owned_start(&album(), None, true, false));
        let deadline = Instant::now() + Duration::from_secs(2);
        while writes(&server).is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(writes(&server).len(), 1, "{kind}");
        let mut clock = active_context_clock();
        clock.sequence = 2;
        match kind {
            "nil" => {
                output.observe(None, Instant::now()).unwrap();
            }
            "foreign" | "restriction" => {
                let mut base = active_context_base();
                if kind == "restriction" {
                    base.devices[0].is_restricted = true;
                } else {
                    base.playback = output_base(2).playback;
                    base.playback.as_mut().unwrap().device.as_mut().unwrap().id =
                        "foreign-device".into();
                }
                output.ingest(base, Instant::now());
            }
            "runtime" => {
                clock.generation = 2;
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "device" => {
                clock.device_id = "another-device".into();
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "media" => {
                clock.media_id = output_clock(2).media_id;
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "context" => {
                clock.metadata.as_mut().unwrap().context_uri = None;
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "epoch" => {
                clock.queue_epoch = Some(8);
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "backward uptime" => {
                clock.control_uptime_ms = Some(1);
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            "inactive" => {
                clock.active = false;
                output.observe(Some(clock), Instant::now()).unwrap();
            }
            _ => {}
        }
        {
            let mut mailbox = shared.0.lock().unwrap();
            match kind {
                "pause" => mailbox.controls.push_back(RemoteControl::Pause.into()),
                "selection" => mailbox.controls.push_back(song().into()),
                "transfer" => mailbox.controls.push_back(
                    RemoteControl::Transfer {
                        device_id: "another-device".into(),
                        operation_id: Some(12),
                    }
                    .into(),
                ),
                "credentials" => mailbox.generation += 1,
                "auth" => mailbox.authorization_revoked = true,
                "pending foreign discovery" => {
                    let mut foreign = active_context_base().devices.remove(0);
                    foreign.id = "foreign-device".into();
                    mailbox.devices = Some(vec![foreign]);
                }
                "pending restricted discovery" => {
                    let mut restricted = active_context_base().devices.remove(0);
                    restricted.is_restricted = true;
                    mailbox.devices = Some(vec![restricted]);
                }
                "pending merged foreign" => {
                    let mut state = active_context_base();
                    let mut foreign = state.devices[0].clone();
                    foreign.id = "foreign-device".into();
                    state.devices.push(foreign);
                    mailbox.state = Some(state);
                }
                "auxiliary failure" => mailbox.auxiliary_failure = Some(api::ApiFailure::network()),
                _ => {
                    output.recheck_direct_owned_start(mailbox.direct_owned_start.as_mut().unwrap())
                }
            }
        }
        server.hold_playback.store(false, Ordering::SeqCst);
        assert!(request.join().unwrap().is_err(), "{kind}");
        assert_eq!(writes(&server).len(), 1, "{kind}");
    }
}

#[test]
fn active_context_404_rechecks_after_transfer_before_retrying_play() {
    let server = Server::new();
    server.mode.store(REJECT_FIRST_PLAY, Ordering::SeqCst);
    server.delay.store(125, Ordering::SeqCst);
    let (api, shared, _) = active_direct_api(&server, &album());
    let request = thread::spawn(move || api.control_with_owned_start(&album(), None, true, false));
    let deadline = Instant::now() + Duration::from_secs(2);
    while writes(&server).len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writes(&server).len(), 2);
    shared
        .0
        .lock()
        .unwrap()
        .controls
        .push_back(RemoteControl::Pause.into());
    assert_eq!(request.join().unwrap().err().unwrap().status, 499);
    assert_eq!(writes(&server).len(), 2);
}

#[test]
fn active_context_origin_cannot_be_rearmed_after_loss_or_its_original_expiry() {
    for expired in [false, true] {
        let now = Instant::now();
        let admitted = if expired {
            now - Duration::from_secs(11)
        } else {
            now
        };
        let mut output = output_clock::OutputProjection::default();
        output.ingest(active_context_base(), admitted);
        output
            .observe(Some(active_context_clock()), admitted)
            .unwrap();
        let mut start = output.remember_http_selection(&album(), admitted).unwrap();
        if !expired {
            output.observe(None, now).unwrap();
            output.recheck_direct_owned_start(&mut start);
        }
        let mut fresh = active_context_clock();
        fresh.sequence = 2;
        fresh.control_uptime_ms = Some(200);
        output.observe(Some(fresh), now).unwrap();
        output.recheck_direct_owned_start(&mut start);
        assert!(start.observed_at.is_none());
        assert!(
            !start
                .active_context_origin
                .as_ref()
                .unwrap()
                .permits_dispatch()
        );
    }
}

#[test]
fn active_context_checks_pending_discovery_before_the_first_write_without_treating_omission_as_takeover()
 {
    for foreign in [false, true] {
        let server = Server::new();
        let (api, shared, _) = active_direct_api(&server, &album());
        let mut devices = vec![];
        if foreign {
            let mut device = active_context_base().devices.remove(0);
            device.id = "foreign-device".into();
            devices.push(device);
        }
        shared.0.lock().unwrap().devices = Some(devices);
        let result = api.control_with_owned_start(&album(), None, true, false);
        assert_eq!(result.is_err(), foreign);
        assert_eq!(writes(&server).len(), usize::from(!foreign));
        if foreign {
            let mut mailbox = shared.0.lock().unwrap();
            mailbox.devices = None;
            assert!(
                !mailbox
                    .direct_owned_start
                    .as_ref()
                    .unwrap()
                    .active_context_origin
                    .as_ref()
                    .unwrap()
                    .permits_dispatch()
            );
        }
    }
}
