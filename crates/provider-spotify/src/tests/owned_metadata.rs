use super::*;

mod capture_ownership;

fn inactive_context_clock(sequence: u64, state: PlaybackState) -> RemoteOutputClock {
    let item = mapping::track(&track('1')).unwrap();
    let mut clock = output_clock(sequence);
    clock.active = state != PlaybackState::Paused;
    clock.state = state;
    clock.rate = if state == PlaybackState::Playing {
        1.0
    } else {
        0.0
    };
    clock.position_ms = if state == PlaybackState::Paused {
        11_978
    } else {
        0
    };
    clock.control_uptime_ms = Some(1_000 + sequence * 100);
    clock.queue_epoch = Some(if state == PlaybackState::Paused {
        40
    } else {
        41
    });
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        next: true,
        ..Default::default()
    });
    clock.metadata = Some(RemoteOutputMetadata {
        title: item.title,
        artists: item.artists,
        album: None,
        duration_ms: item.duration_ms,
        artwork: None,
        context_uri: Some(format!("spotify:album:{}", "a".repeat(22))),
    });
    clock
}

fn begin_inactive_context_restart(
    now: Instant,
    origin: RemoteOutputClock,
) -> (output_clock::OutputProjection, RemoteState) {
    let mut output = output_clock::OutputProjection::default();
    let mut base = output_base(1);
    let frame_device = base.playback.as_mut().unwrap().device.as_mut().unwrap();
    let mut owned = frame_device.clone();
    owned.is_active = false;
    base.devices.push(owned);
    // A cold Web API 204 has no playback frame to fall back to.
    base.playback = None;
    output.ingest(base.clone(), now);
    output.observe(Some(origin), now).unwrap();
    output.take(now).unwrap();
    let _ = output.remember_http_selection(
        &RemoteControl::StartContext {
            uri: format!("spotify:album:{}", "a".repeat(22)),
            device_id: "fixture-device".into(),
            operation_id: Some(25),
            item: None,
            position: None,
            query_id: None,
        },
        now,
    );
    (output, base)
}

fn acknowledge_inactive_context_restart(
    output: &mut output_clock::OutputProjection,
    base: &mut RemoteState,
    now: Instant,
) {
    base.control_result = Some(RemoteControlResult {
        operation_id: 25,
        error: None,
    });
    output.ingest(base.clone(), now);
}

fn observe_inactive_context_restart(
    output: &mut output_clock::OutputProjection,
    clock: RemoteOutputClock,
    now: Instant,
) -> RemoteState {
    output.observe(Some(clock), now).unwrap();
    output.take(now).unwrap()
}

#[test]
fn inactive_same_media_context_restart_becomes_visible_after_full_buffering_and_ack() {
    for ack_before_loading in [false, true] {
        let now = Instant::now();
        let (mut output, mut base) =
            begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
        if ack_before_loading {
            acknowledge_inactive_context_restart(&mut output, &mut base, now);
        }
        let loading = inactive_context_clock(2, PlaybackState::Loading);
        assert!(
            !observe_inactive_context_restart(
                &mut output,
                loading,
                now + Duration::from_millis(100)
            )
            .owned_output_observed
        );
        if !ack_before_loading {
            acknowledge_inactive_context_restart(
                &mut output,
                &mut base,
                now + Duration::from_millis(150),
            );
        }
        let mut playing = inactive_context_clock(3, PlaybackState::Playing);
        playing.position_ms = 10;
        let confirmed = observe_inactive_context_restart(
            &mut output,
            playing.clone(),
            now + Duration::from_millis(200),
        );
        assert!(confirmed.owned_output_observed);
        assert!(!confirmed.owned_output_control && !output.has_owned_binding());
        assert!(
            output
                .control_clock(now + Duration::from_millis(200))
                .is_none()
        );
        assert!(
            output
                .http_navigation_clock(now + Duration::from_millis(200))
                .is_none()
        );
        let frame = confirmed.playback.unwrap();
        assert_eq!(frame.current.unwrap().id, playing.media_id.clone().unwrap());
        assert_eq!(
            frame.context_uri,
            playing.metadata.as_ref().unwrap().context_uri
        );
        assert_eq!(frame.position_ms, 10);

        // The user's deferred Pause may now act on this visible selection.
        // A confirmed occurrence stays visible when that same occurrence pauses.
        playing.sequence += 1;
        playing.state = PlaybackState::Paused;
        playing.rate = 0.0;
        playing.control_uptime_ms = Some(1_400);
        assert!(
            observe_inactive_context_restart(
                &mut output,
                playing.clone(),
                now + Duration::from_millis(250)
            )
            .owned_output_observed
        );

        // A later occurrence of the same URI cannot inherit this witness.
        playing.sequence += 1;
        playing.queue_epoch = Some(42);
        assert!(
            !observe_inactive_context_restart(
                &mut output,
                playing,
                now + Duration::from_millis(300)
            )
            .owned_output_observed
        );
    }
}

#[test]
fn inactive_same_media_restart_rejects_ambiguous_or_mixed_occurrence_evidence() {
    for fault in [
        "active origin",
        "missing origin actions",
        "missing origin metadata",
        "missing origin epoch",
        "origin at zero",
        "no loading",
        "partial loading",
        "unchanged loading epoch",
        "distant loading",
        "wrong loading context",
        "wrong loading media",
        "wrong loading device",
        "wrong loading generation",
        "stale loading sequence",
        "backward loading uptime",
        "missing playing actions",
        "missing playing metadata",
        "wrong playing epoch",
        "distant playing",
        "backward playing uptime",
        "other same-media command",
        "paused before playing",
        "stale playing",
        "restricted device",
        "authentication lost",
        "missing ack",
        "wrong ack",
        "failed ack",
        "replaced selection",
    ] {
        let now = Instant::now();
        let mut origin = inactive_context_clock(1, PlaybackState::Paused);
        match fault {
            "active origin" => origin.active = true,
            "missing origin actions" => origin.actions = None,
            "missing origin metadata" => origin.metadata = None,
            "missing origin epoch" => origin.queue_epoch = None,
            "origin at zero" => origin.position_ms = 0,
            _ => {}
        }
        let (mut output, mut base) = begin_inactive_context_restart(now, origin);
        let mut loading = inactive_context_clock(2, PlaybackState::Loading);
        match fault {
            "partial loading" => loading.actions = None,
            "unchanged loading epoch" => loading.queue_epoch = Some(40),
            "distant loading" => loading.position_ms = 4_000,
            "wrong loading context" => {
                loading.metadata.as_mut().unwrap().context_uri =
                    Some(format!("spotify:album:{}", "b".repeat(22)))
            }
            "wrong loading media" => {
                loading.media_id = Some(mapping::track(&track('2')).unwrap().id)
            }
            "wrong loading device" => loading.device_id = "other-output".into(),
            "wrong loading generation" => loading.generation = 2,
            "stale loading sequence" => loading.sequence = 1,
            "backward loading uptime" => loading.control_uptime_ms = Some(1_099),
            _ => {}
        }
        if fault != "no loading" {
            output
                .observe(Some(loading), now + Duration::from_millis(100))
                .unwrap();
            let _ = output.take(now + Duration::from_millis(100));
        }
        if fault == "wrong ack" || fault == "failed ack" {
            base.control_result = Some(RemoteControlResult {
                operation_id: if fault == "wrong ack" { 24 } else { 25 },
                error: (fault == "failed ack")
                    .then(|| PlayerError::invalid("fixture start rejected")),
            });
            output.ingest(base.clone(), now + Duration::from_millis(150));
        } else if fault != "missing ack" {
            acknowledge_inactive_context_restart(
                &mut output,
                &mut base,
                now + Duration::from_millis(150),
            );
        }
        if fault == "replaced selection" {
            let _ = output.remember_http_selection(
                &RemoteControl::StartContext {
                    uri: format!("spotify:album:{}", "a".repeat(22)),
                    device_id: "fixture-device".into(),
                    operation_id: Some(26),
                    item: None,
                    position: None,
                    query_id: None,
                },
                now + Duration::from_millis(160),
            );
        }
        if fault == "paused before playing" {
            let mut paused = inactive_context_clock(3, PlaybackState::Paused);
            paused.active = true;
            paused.queue_epoch = Some(41);
            observe_inactive_context_restart(&mut output, paused, now + Duration::from_millis(175));
        }
        let mut playing = inactive_context_clock(4, PlaybackState::Playing);
        playing.position_ms = 10;
        match fault {
            "missing playing actions" => playing.actions = None,
            "missing playing metadata" => playing.metadata = None,
            "wrong playing epoch" => playing.queue_epoch = Some(42),
            "distant playing" => playing.position_ms = 11_978,
            "backward playing uptime" => playing.control_uptime_ms = Some(1_199),
            "other same-media command" => playing.confirmed_same_media_next = Some(99),
            "restricted device" => {
                base.devices[0].is_restricted = true;
                output.ingest(base, now + Duration::from_millis(180));
            }
            "authentication lost" => {
                base.authentication_required = true;
                output.ingest(base, now + Duration::from_millis(180));
            }
            _ => {}
        }
        output
            .observe(Some(playing), now + Duration::from_millis(200))
            .unwrap();
        let read_at =
            now + Duration::from_millis(if fault == "stale playing" { 1_101 } else { 200 });
        assert!(
            !output.take(read_at).unwrap().owned_output_observed,
            "{fault}"
        );
        assert!(!output.has_owned_binding(), "{fault}");
    }
}

#[test]
fn inactive_same_media_restart_does_not_reinterpret_playing_before_http_ack() {
    let now = Instant::now();
    let (mut output, mut base) =
        begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
    observe_inactive_context_restart(
        &mut output,
        inactive_context_clock(2, PlaybackState::Loading),
        now + Duration::from_millis(100),
    );
    assert!(
        !observe_inactive_context_restart(
            &mut output,
            inactive_context_clock(3, PlaybackState::Playing),
            now + Duration::from_millis(200)
        )
        .owned_output_observed
    );
    acknowledge_inactive_context_restart(&mut output, &mut base, now + Duration::from_millis(250));
    // Even a later complete snapshot must not turn pre-ACK activation into an
    // acknowledgement of that exact start operation.
    assert!(
        !observe_inactive_context_restart(
            &mut output,
            inactive_context_clock(4, PlaybackState::Playing),
            now + Duration::from_millis(300)
        )
        .owned_output_observed
    );
}

#[test]
fn addressed_http_pause_keeps_the_confirmed_restart_visible_during_a_held_cloud_204() {
    struct ReleaseRead(Arc<AtomicBool>);
    impl Drop for ReleaseRead {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    while provider.poll().is_some() {}
    let now = Instant::now();
    let (mut output, mut base) =
        begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
    observe_inactive_context_restart(
        &mut output,
        inactive_context_clock(2, PlaybackState::Loading),
        now,
    );
    acknowledge_inactive_context_restart(&mut output, &mut base, now);
    let mut playing = inactive_context_clock(3, PlaybackState::Playing);
    playing.position_ms = 10;
    let confirmed = observe_inactive_context_restart(&mut output, playing.clone(), now);
    assert!(confirmed.owned_output_observed && !confirmed.owned_output_control);
    assert!(base.playback.is_none());
    provider.output = output;

    let old_reads = server
        .records()
        .iter()
        .filter(|request| {
            request.method == "GET" && request.target.split('?').next() == Some("/v1/me/player")
        })
        .count();
    server.mode.store(204, Ordering::SeqCst);
    server.hold_playback.store(true, Ordering::SeqCst);
    let _release = ReleaseRead(server.hold_playback.clone());
    provider
        .submit_pause_on_device_with_request_id("fixture-device", 51)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(51));
    provider.output.republish();
    let assert_visible = |update: RemoteUpdate| {
        if let RemoteUpdate::State(state) = update {
            assert!(
                state.owned_output_observed,
                "The addressed Pause dropped its existing display witness"
            );
            assert!(!state.owned_output_control);
            let frame = state
                .playback
                .expect("The confirmed selection flashed empty");
            assert_eq!(
                frame.current.as_ref().map(|item| &item.id),
                playing.media_id.as_ref()
            );
            assert_eq!(
                frame.context_uri,
                playing.metadata.as_ref().unwrap().context_uri
            );
            assert_ne!(frame.state, PlaybackState::Idle);
        } else {
            assert!(!matches!(update, RemoteUpdate::OutputCommand(_)));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(update) = provider.poll() {
            assert_visible(update);
        }
        let records = server.records();
        let paused = records.iter().any(|request| {
            request.method == "PUT"
                && request.target == "/v1/me/player/pause?device_id=fixture-device"
        });
        let held_read = records
            .iter()
            .filter(|request| {
                request.method == "GET" && request.target.split('?').next() == Some("/v1/me/player")
            })
            .count()
            > old_reads;
        if paused && held_read {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "The actual HTTP Pause and its held verification GET were not observed: {:?}",
            records
                .iter()
                .map(|request| (&request.method, &request.target))
                .collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!provider.output.has_owned_binding());
    provider.output.republish();
    assert_visible(
        provider
            .poll()
            .expect("A held cloud read blocked local projection"),
    );

    // The real clock can pause before the cloud's confirming GET returns.
    for (sequence, full) in [(4, false), (5, true)] {
        let mut paused = playing.clone();
        paused.sequence = sequence;
        paused.control_uptime_ms = Some(1_000 + sequence * 100);
        paused.state = PlaybackState::Paused;
        paused.rate = 0.0;
        paused.position_ms = 256;
        if full {
            paused.actions.as_mut().unwrap().pause = false;
            paused.actions.as_mut().unwrap().play = true;
        } else {
            paused.actions = None;
        }
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(paused),
            })
            .unwrap();
        let state = wait(&mut provider, |update| {
            matches!(update, RemoteUpdate::State(_))
        });
        assert_visible(state.clone());
        assert!(
            matches!(state, RemoteUpdate::State(state) if state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused && frame.position_ms == 256))
        );
        assert!(!provider.output.has_owned_binding());
    }
    server.hold_playback.store(false, Ordering::SeqCst);
    let state = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(_))
    });
    assert_visible(state);
    assert!(!provider.output.has_owned_binding());
}

#[test]
fn addressed_pause_cannot_reuse_an_unconfirmed_or_superseded_restart_witness() {
    for interruption in [
        "unconfirmed start",
        "unaddressed pause",
        "foreign pause",
        "seek",
        "new album",
        "stale clock",
        "new occurrence",
    ] {
        let now = Instant::now();
        let (mut output, mut base) =
            begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
        observe_inactive_context_restart(
            &mut output,
            inactive_context_clock(2, PlaybackState::Loading),
            now,
        );
        acknowledge_inactive_context_restart(&mut output, &mut base, now);
        let mut playing = inactive_context_clock(3, PlaybackState::Playing);
        playing.position_ms = 10;
        output.observe(Some(playing.clone()), now).unwrap();
        if interruption != "unconfirmed start" {
            assert!(output.take(now).unwrap().owned_output_observed);
        }
        if interruption == "new occurrence" {
            playing.sequence = 4;
            playing.queue_epoch = Some(42);
            output.observe(Some(playing.clone()), now).unwrap();
        }
        let control = match interruption {
            "unaddressed pause" => RemoteControl::Pause,
            "foreign pause" => RemoteControl::PauseOnDevice {
                device_id: "other-output".into(),
            },
            "seek" => RemoteControl::Seek { position_ms: 1_000 },
            "new album" => RemoteControl::StartContext {
                uri: format!("spotify:album:{}", "b".repeat(22)),
                device_id: "fixture-device".into(),
                operation_id: Some(26),
                item: None,
                position: None,
                query_id: None,
            },
            _ => RemoteControl::PauseOnDevice {
                device_id: "fixture-device".into(),
            },
        };
        let submitted = now
            + if interruption == "stale clock" {
                output_clock::CONTROL_FRESHNESS + Duration::from_millis(1)
            } else {
                Duration::ZERO
            };
        output.remember_selection(&control, submitted);
        let _ = output.remember_http_selection(&control, submitted);
        // Another fresh, otherwise matching old clock cannot revive a display
        // witness revoked by a different intent or an unverified occurrence.
        playing.sequence = 5;
        playing.control_uptime_ms = Some(1_500);
        output
            .observe(Some(playing), submitted + Duration::from_millis(1))
            .unwrap();
        let state = output.take(submitted + Duration::from_millis(1)).unwrap();
        assert!(!state.owned_output_observed, "{interruption}");
        assert!(!output.has_owned_binding(), "{interruption}");
    }
}

#[test]
fn cold_context_ack_before_activation_cannot_project_the_previous_media_as_the_new_album() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    let mut base = output_base(1);
    let device = base.playback.as_mut().unwrap().device.as_mut().unwrap();
    let mut owned = device.clone();
    owned.is_active = false;
    base.devices.push(owned);
    device.id = "external-output".into();
    output.ingest(base.clone(), now);

    let album = format!("spotify:album:{}", "a".repeat(22));
    let item = mapping::track(&track('1')).unwrap();
    let mut clock = output_clock(1);
    clock.active = false;
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.position_ms = 2_000;
    clock.control_uptime_ms = Some(100);
    clock.metadata = Some(RemoteOutputMetadata {
        title: item.title,
        artists: item.artists,
        album: None,
        duration_ms: item.duration_ms,
        artwork: None,
        context_uri: Some(format!("spotify:album:{}", "b".repeat(22))),
    });
    output.observe(Some(clock.clone()), now).unwrap();
    output.take(now).unwrap();
    let selection = RemoteControl::StartContext {
        uri: album.clone(),
        device_id: "fixture-device".into(),
        operation_id: Some(5),
        item: None,
        position: None,
        query_id: None,
    };
    assert!(output.remember_http_selection(&selection, now).is_some());
    base.control_result = Some(RemoteControlResult {
        operation_id: 5,
        error: None,
    });
    output.ingest(base, now);

    clock.sequence += 1;
    clock.active = true;
    clock.state = PlaybackState::Playing;
    clock.rate = 1.0;
    clock.metadata.as_mut().unwrap().context_uri = Some(album.clone());
    output.observe(Some(clock.clone()), now).unwrap();
    assert!(!output.take(now).unwrap().owned_output_observed);

    clock.sequence += 1;
    clock.position_ms = 1;
    clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
    output.observe(Some(clock.clone()), now).unwrap();
    let confirmed = output.take(now).unwrap();
    assert!(confirmed.owned_output_observed && !confirmed.owned_output_control);
    let playback = confirmed.playback.unwrap();
    assert_eq!(
        playback.current.as_ref().map(|item| &item.id),
        clock.media_id.as_ref()
    );
    assert_eq!(playback.context_uri.as_deref(), Some(album.as_str()));
}

#[test]
fn acknowledged_cold_album_projects_only_the_fresh_owned_target_before_http_catches_up() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    let mut base = output_base(1);
    let mut device = base.playback.as_ref().unwrap().device.clone().unwrap();
    device.is_active = false;
    base.devices.push(device);
    let old_id = base
        .playback
        .as_ref()
        .unwrap()
        .current
        .as_ref()
        .unwrap()
        .id
        .clone();
    let album = format!("spotify:album:{}", "a".repeat(22));
    let clock = |sequence, letter, context: &str, state, active| {
        let item = mapping::track(&track(letter)).unwrap();
        let mut clock = output_clock(sequence);
        clock.media_id = Some(item.id);
        clock.state = state;
        clock.active = active;
        clock.rate = if state == PlaybackState::Playing {
            1.0
        } else {
            0.0
        };
        clock.position_ms = 1;
        clock.control_uptime_ms = Some(sequence);
        clock.actions = Some(RemoteOutputActions {
            pause: true,
            next: true,
            ..Default::default()
        });
        clock.metadata = Some(RemoteOutputMetadata {
            title: item.title,
            artists: item.artists,
            album: None,
            duration_ms: item.duration_ms,
            artwork: None,
            context_uri: Some(context.to_owned()),
        });
        clock
    };
    let prove = |output: &mut output_clock::OutputProjection, clock| {
        output.observe(Some(clock), now).unwrap();
        output.take(now).unwrap()
    };
    output.ingest(base.clone(), now);
    prove(
        &mut output,
        clock(1, '1', &album, PlaybackState::Paused, false),
    );
    let control = RemoteControl::StartContext {
        uri: album.clone(),
        device_id: "fixture-device".into(),
        operation_id: Some(5),
        item: None,
        position: None,
        query_id: None,
    };
    let direct = output.remember_http_selection(&control, now).unwrap();
    assert_eq!(direct.operation, 5);
    assert_eq!(direct.device, "fixture-device");
    assert_eq!(direct.observed_at, Some(now));
    let mut wrong_runtime = direct;
    wrong_runtime.generation += 1;
    output.recheck_direct_owned_start(&mut wrong_runtime);
    assert_eq!(wrong_runtime.observed_at, None);
    let activation = prove(
        &mut output,
        clock(2, '2', &album, PlaybackState::Playing, true),
    );
    assert!(!activation.owned_output_observed);
    base.control_result = Some(RemoteControlResult {
        operation_id: 4,
        error: None,
    });
    output.ingest(base.clone(), now);
    let stale_receipt = prove(
        &mut output,
        clock(3, '2', &album, PlaybackState::Playing, true),
    );
    assert!(!stale_receipt.owned_output_observed);
    base.control_result = Some(RemoteControlResult {
        operation_id: 5,
        error: None,
    });
    output.ingest(base.clone(), now);
    let stale_item = prove(
        &mut output,
        clock(4, '2', &album, PlaybackState::Playing, true),
    );
    assert!(!stale_item.owned_output_observed);
    assert_eq!(stale_item.playback.unwrap().current.unwrap().id, old_id);
    let loading = prove(
        &mut output,
        clock(5, '3', &album, PlaybackState::Loading, true),
    );
    assert!(!loading.owned_output_observed);
    let wrong = prove(
        &mut output,
        clock(
            6,
            '3',
            "spotify:album:bbbbbbbbbbbbbbbbbbbbbb",
            PlaybackState::Playing,
            true,
        ),
    );
    assert!(!wrong.owned_output_observed);
    let target = clock(7, '3', &album, PlaybackState::Playing, true);
    let observed = prove(&mut output, target.clone());
    assert!(observed.owned_output_observed);
    assert!(!observed.owned_output_control && !output.has_owned_binding());
    let playback = observed.playback.unwrap();
    assert_eq!(
        playback.current.as_ref().map(|item| &item.id),
        target.media_id.as_ref()
    );
    assert_eq!(playback.context_uri.as_deref(), Some(album.as_str()));
    assert!(playback.device.as_ref().unwrap().is_active);
    assert!(!playback.capabilities.pause && !playback.transport.next);

    let changed = prove(
        &mut output,
        clock(8, '4', &album, PlaybackState::Playing, true),
    );
    assert!(!changed.owned_output_observed);
    let observed_again = prove(
        &mut output,
        clock(9, '3', &album, PlaybackState::Playing, true),
    );
    assert!(!observed_again.owned_output_observed);

    base.playback = Some(playback);
    base.observation_id = 2;
    output.ingest(base, now);
    assert!(output.take(now).unwrap().owned_output_control);
    assert!(output.has_owned_binding());
}

#[test]
fn verified_owned_metadata_needs_no_extra_read_but_keeps_http_verification() {
    let mut provider = SpotifyRemoteProvider {
        output: output_clock::OutputProjection::default(),
        owned_commands: owned_commands::OwnedCommands::default(),
        autoplay: autoplay::Autoplay::default(),
        autoplay_trace: None,
        shared: Arc::new((Mutex::new(Mailbox::default()), Condvar::new())),
        worker: None,
        auxiliary: None,
    };
    provider.output.ingest(output_base(1), Instant::now());
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(output_clock(1)),
        })
        .unwrap();
    provider.poll();

    let item = mapping::track(&track('2')).unwrap();
    let mut observed = output_clock(2);
    observed.media_id = Some(item.id.clone());
    observed.metadata = Some(RemoteOutputMetadata {
        title: item.title,
        artists: item.artists,
        album: None,
        duration_ms: item.duration_ms,
        artwork: None,
        context_uri: None,
    });
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(observed.clone()),
        })
        .unwrap();
    assert!(!provider.shared.0.lock().unwrap().refresh);
    assert!(matches!(provider.poll(), Some(RemoteUpdate::State(state))
        if state.playback.as_ref().and_then(|frame| frame.current.as_ref()).is_some_and(|current| current.id == item.id)));

    observed.sequence = 3;
    observed.metadata.as_mut().unwrap().duration_ms = 0;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(observed.clone()),
        })
        .unwrap();
    {
        let mut mailbox = provider.shared.0.lock().unwrap();
        assert!(
            mailbox.refresh,
            "Lost metadata must request a read even for the same URI"
        );
        mailbox.refresh = false;
        mailbox.http_mutating = true;
    }
    observed.sequence = 4;
    observed.metadata.as_mut().unwrap().duration_ms = item.duration_ms;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(observed),
        })
        .unwrap();
    assert!(
        provider.shared.0.lock().unwrap().refresh,
        "An HTTP start keeps priority verification"
    );
}

#[test]
fn chapter_details_repair_episode_display_metadata_with_bounded_account_scoped_records() {
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let mut canonical = mapping::track(&spoken_fixture("episode")).unwrap();
    canonical.artists = vec!["Original book".into()];
    let mut base = output_base(1);
    base.playback.as_mut().unwrap().current = Some(canonical.clone());
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.remember_selection(
        &RemoteControl::StartOnDevice {
            item: chapter.clone(),
            device_id: "fixture-device".into(),
            operation_id: Some(1),
        },
        now,
    );
    output.ingest(base.clone(), now);
    let selected = output.take(now).unwrap().playback.unwrap();
    assert_eq!(selected.current.as_ref().unwrap().id, chapter.id);
    assert_eq!(selected.current.as_ref().unwrap().artists, chapter.artists);
    assert!(!selected.current.unwrap().capabilities.library_save);

    let server = Server::new();
    let mut api = fixture_api(&server);
    let mut page = api
        .catalog(&RemoteQuery {
            query_id: 12,
            kind: RemoteCatalogKind::Chapter,
            text: "e".repeat(22),
            offset: 0,
        })
        .unwrap_or_else(|_| panic!("chapter details"));
    page.items[0].artists = vec!["Fresh original author".into()];
    output.remember_catalog(&page);
    let refreshed = output.take(now).unwrap().playback.unwrap();
    assert_eq!(
        refreshed.current.unwrap().artists,
        ["Fresh original author"]
    );
    page.items[0].id = MediaId::parse(format!("spotify:chapter:{}", "d".repeat(22))).unwrap();
    page.items[0].artists = vec!["Unrelated author".into()];
    output.remember_catalog(&page);
    let mut unavailable = base.clone();
    unavailable
        .playback
        .as_mut()
        .unwrap()
        .current
        .as_mut()
        .unwrap()
        .capabilities
        .play = false;
    unavailable.playback.as_mut().unwrap().capabilities.play = false;
    output.ingest(unavailable, now);
    let restricted = output.take(now).unwrap().playback.unwrap();
    let item = restricted.current.unwrap();
    assert_eq!(item.artists, ["Fresh original author"]);
    assert!(!item.capabilities.play && !restricted.capabilities.play);
    assert!(!item.capabilities.pcm_analysis && !item.capabilities.system_media_session);
    assert_eq!(
        server.records().len(),
        1,
        "Metadata projection must not perform an HTTP lookup per frame"
    );

    output.reset();
    output.ingest(base, now);
    let reset = output.take(now).unwrap().playback.unwrap().current.unwrap();
    assert_eq!(reset.id, canonical.id);
    assert_eq!(
        reset.artists,
        ["Original book"],
        "New authorization cannot reuse old account chapter metadata"
    );
}

#[test]
fn a_pending_chapter_keeps_the_current_author_until_the_new_record_is_observed() {
    let current = mapping::track(&spoken_fixture("chapter")).unwrap();
    let mut next = current.clone();
    next.id = MediaId::parse(format!("spotify:chapter:{}", "d".repeat(22))).unwrap();
    next.artists = vec!["Next original author".into()];
    let start = |item| RemoteControl::StartOnDevice {
        item,
        device_id: "fixture-device".into(),
        operation_id: Some(1),
    };
    let mut base = output_base(1);
    base.playback.as_mut().unwrap().current = mapping::track(&spoken_fixture("episode"));
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.remember_selection(&start(current.clone()), now);
    output.ingest(base.clone(), now);
    assert_eq!(
        output
            .take(now)
            .unwrap()
            .playback
            .unwrap()
            .current
            .unwrap()
            .artists,
        current.artists
    );
    output.remember_selection(&start(next.clone()), now);
    assert!(
        output.take(now).is_none(),
        "Admission alone must not announce a new playback observation"
    );
    output.ingest(base.clone(), now);
    assert_eq!(
        output
            .take(now)
            .unwrap()
            .playback
            .unwrap()
            .current
            .unwrap()
            .artists,
        current.artists
    );
    base.playback.as_mut().unwrap().current.as_mut().unwrap().id =
        MediaId::parse(format!("spotify:episode:{}", "d".repeat(22))).unwrap();
    output.ingest(base, now);
    let observed = output.take(now).unwrap().playback.unwrap().current.unwrap();
    assert_eq!(observed.id, next.id);
    assert_eq!(observed.artists, next.artists);
}

fn next_clock(sequence: u64) -> RemoteOutputClock {
    let mut clock = output_clock(sequence);
    clock.media_id = Some(MediaId::parse(format!("spotify:track:{}", "2".repeat(22))).unwrap());
    clock.position_ms = 500;
    clock.metadata = Some(RemoteOutputMetadata {
        title: "Original Soloist fixture".into(),
        artists: vec!["Original artist".into()],
        album: Some("Original album".into()),
        duration_ms: 65_000,
        artwork: Some("https://i.scdn.co/image/original-fixture".into()),
        context_uri: Some(format!("spotify:album:{}", "m".repeat(22))),
    });
    clock
}

#[test]
fn a_verified_output_projects_new_metadata_before_http_and_prefers_confirmed_http_metadata() {
    let mut output = output_clock::OutputProjection::default();
    let now = Instant::now();
    let mut base = output_base(1);
    base.playback.as_mut().unwrap().upcoming = vec![mapping::track(&track('3')).unwrap()];
    output.ingest(base, now);
    let next = next_clock(1);
    assert!(output.observe(Some(next.clone()), now).unwrap());
    let unbound = output.take(now).unwrap();
    assert!(!unbound.owned_output_observed);
    assert_ne!(
        unbound.playback.unwrap().current.unwrap().id,
        next.media_id.clone().unwrap()
    );

    output.observe(Some(output_clock(2)), now).unwrap();
    assert!(output.take(now).unwrap().owned_output_observed);
    let mut next = next_clock(3);
    next.actions = Some(output_actions());
    output.observe(Some(next.clone()), now).unwrap();
    let owned = output.take(now).unwrap();
    assert!(owned.owned_output_observed);
    let playback = owned.playback.unwrap();
    let item = playback.current.unwrap();
    assert_eq!(Some(&item.id), next.media_id.as_ref());
    assert_eq!(item.title, "Original Soloist fixture");
    assert_eq!(item.artists, ["Original artist"]);
    assert_eq!(playback.position_ms, 500);
    assert!(playback.upcoming.is_empty());
    assert!(playback.capabilities.seek);
    assert!(!playback.capabilities.pcm_analysis && !playback.capabilities.system_media_session);

    let mut raw = frame();
    raw["item"] = track('2');
    let mut caught_up = output_base(2);
    caught_up.playback = Some(mapping::playback(&raw));
    output.ingest(caught_up, now);
    let result = output.take(now).unwrap();
    assert!(result.owned_output_observed);
    assert_eq!(
        result.playback.unwrap().current.unwrap().title,
        "Original fixture tone"
    );
}

#[test]
fn a_bound_output_keeps_observing_during_quota_but_needs_fresh_binding_after_expiry() {
    let mut output = output_clock::OutputProjection::default();
    let now = Instant::now();
    output.ingest(output_base(1), now);
    output.observe(Some(output_clock(1)), now).unwrap();
    assert!(output.take(now).unwrap().owned_output_observed);
    output.set_rate_limit(Some(RateLimit {
        seconds: 30,
        developer_quota: false,
    }));
    output.observe(Some(next_clock(2)), now).unwrap();
    let offline = output.take(now).unwrap();
    assert_eq!(offline.connection, ProviderConnectionState::Unavailable);
    assert_eq!(offline.retry_after_seconds, Some(30));
    assert!(offline.owned_output_observed);
    assert_eq!(
        offline.playback.unwrap().current.unwrap().title,
        "Original Soloist fixture"
    );

    let expired = now + Duration::from_secs(7);
    output.observe(Some(next_clock(3)), expired).unwrap();
    let unbound = output.take(expired).unwrap();
    assert!(!unbound.owned_output_observed);
    assert_eq!(
        unbound.playback.unwrap().current.unwrap().title,
        "Original fixture tone"
    );
}

#[test]
fn invalid_metadata_is_dropped_and_an_external_device_revokes_the_binding() {
    let mut output = output_clock::OutputProjection::default();
    let now = Instant::now();
    output.ingest(output_base(1), now);
    output.observe(Some(output_clock(1)), now).unwrap();
    assert!(output.take(now).unwrap().owned_output_observed);
    let mut invalid = next_clock(2);
    invalid.metadata.as_mut().unwrap().artwork =
        Some("https://i.scdn.co/image/cover?private=1".into());
    output.observe(Some(invalid), now).unwrap();
    assert!(!output.take(now).unwrap().owned_output_observed);

    let mut external = output_base(2);
    external
        .playback
        .as_mut()
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .id = "external".into();
    output.ingest(external, now);
    output.observe(Some(next_clock(3)), now).unwrap();
    let result = output.take(now).unwrap();
    assert!(!result.owned_output_observed);
    assert_eq!(result.playback.unwrap().device.unwrap().id, "external");
    output.ingest(output_base(3), now);
    assert!(!output.take(now).unwrap().owned_output_observed);
}
