use super::*;

const POSITION: u64 = 3456;
const OPERATION: u64 = 73;

fn native_resume_clock(
    control: &RemoteControl,
    sequence: u64,
    state: PlaybackState,
) -> RemoteOutputClock {
    let RemoteControl::ResumeOnDevice {
        item,
        device_id,
        position_ms,
        context_uri,
        ..
    } = control
    else {
        unreachable!()
    };
    let mut clock = output_clock(sequence);
    clock.device_id.clone_from(device_id);
    clock.media_id = Some(item.id.clone());
    clock.state = state;
    clock.rate = if state == PlaybackState::Playing {
        1.0
    } else {
        0.0
    };
    clock.position_ms = *position_ms;
    clock.control_uptime_ms = Some(sequence * 100);
    clock.queue_epoch = Some(7);
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        ..Default::default()
    });
    clock.metadata = Some(RemoteOutputMetadata {
        title: item.title.clone(),
        artists: item.artists.clone(),
        album: None,
        duration_ms: item.duration_ms,
        artwork: None,
        context_uri: context_uri.clone(),
    });
    clock
}

pub(super) fn probe_path(path: &str) -> bool {
    path.starts_with("/v1/playlists/") && path.ends_with("/items")
        || path.starts_with("/v1/albums/") && path.ends_with("/tracks")
}

pub(super) fn probe_response(path: &str, offset: &str, code: &mut u16) -> Value {
    let row = |id: char| json!({"uri":format!("spotify:track:{}",id.to_string().repeat(22))});
    let offset: u64 = offset.parse().unwrap();
    if path.ends_with("/tracks") {
        return match path.split('/').nth(3).and_then(|id| id.chars().next()) {
            Some('a') => json!({"offset":offset,"total":3,"items":[row('1'),row('4'),row('2')]}),
            Some('b') => json!({"offset":offset,"total":2,"items":[row('1'),row('2')]}),
            Some('c') if offset == 50 => {
                json!({"offset":offset,"total":52,"items":[row('1'),row('4')]})
            }
            Some('c') => json!({"offset":offset,"total":52,"items":vec![row('1');50]}),
            Some('i') => {
                json!({"offset":offset,"total":2,"items":[row('1'),{"id":"4".repeat(22)}]})
            }
            Some('m' | 'f') => {
                *code = 403;
                Value::Null
            }
            Some('l') => json!({"total":1,"items":[row('4')]}),
            other => panic!("Unexpected album context fixture {other:?}"),
        };
    }
    match path.split('/').nth(3).and_then(|id| id.chars().next()) {
        Some('a') => json!({"offset":offset,"total":2,"items":[{"item":null},{"item":row('4')}]}),
        Some('b') => {
            json!({"offset":offset,"total":2,"items":[{"item":row('1')},{"item":row('2')}]})
        }
        Some('c') if offset == 50 => {
            json!({"offset":offset,"total":51,"items":[{"track":row('4')}]})
        }
        Some('d') if offset == 100 => {
            json!({"offset":offset,"total":101,"items":[{"item":row('4')}]})
        }
        Some(id @ ('c' | 'd')) => {
            let total = if id == 'c' { 51 } else { 101 };
            json!({"offset":offset,"total":total,"items":vec![json!({"item":row('1')});50]})
        }
        Some('e') => {
            let mut unavailable = row('4');
            unavailable["is_playable"] = json!(false);
            json!({"offset":offset,"total":1,"items":[{"item":unavailable}]})
        }
        Some('f') => {
            *code = 403;
            Value::Null
        }
        Some('m') => json!({"offset":offset,"items":{}}),
        other => panic!("Unexpected context fixture {other:?}"),
    }
}

fn resume(kind: &str, id: char) -> RemoteControl {
    RemoteControl::ResumeOnDevice {
        operation_id: Some(OPERATION),
        item: mapping::track(&track('4')).unwrap(),
        device_id: "owned-device".into(),
        position_ms: POSITION,
        context_uri: Some(format!("spotify:{kind}:{}", id.to_string().repeat(22))),
    }
}

fn assert_probes(records: &[Request], kind: &str, count: usize) {
    assert_eq!(records.len(), count);
    for (page, request) in records.iter().enumerate() {
        assert_eq!(request.method, "GET");
        assert!(request.authorized);
        let url = reqwest::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        assert!(probe_path(url.path()));
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "limit" && value == "50")
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "offset" && value == (page * 50).to_string())
        );
        if kind == "playlist" {
            assert!(url.path().ends_with("/items"));
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "additional_types" && value == "track,episode")
            );
            assert!(url.query_pairs().any(|(key, value)| key == "fields"
                && value.contains("item(uri,")
                && value.contains("track(uri,")));
        } else {
            assert!(url.path().ends_with("/tracks"));
        }
    }
}

fn assert_play(request: &Request, body: Value) {
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/v1/me/player/play?device_id=owned-device");
    assert_eq!(request.body, body);
}

fn album_memo_fixture(catalog: &Server, transport: &Server) -> (SpotifyRemoteProvider, Api, Api) {
    let shared = Arc::new((
        Mutex::new(Mailbox {
            generation: 7,
            ..Mailbox::default()
        }),
        Condvar::new(),
    ));
    let client = |server: &Server| {
        let mut api = Api::new(server.base.clone(), shared.clone()).unwrap();
        api.generation = 7;
        api.token = Some("fixture-bearer".into());
        api
    };
    let (catalog, transport) = (client(catalog), client(transport));
    let provider = SpotifyRemoteProvider {
        shared,
        output: output_clock::OutputProjection::default(),
        owned_commands: owned_commands::OwnedCommands::default(),
        autoplay: autoplay::Autoplay::default(),
        autoplay_trace: None,
        worker: None,
        auxiliary: None,
    };
    (provider, catalog, transport)
}

fn read_album_memo_track(catalog: &mut Api, track_id: char) {
    catalog
        .catalog(&track_detail_query(track_id))
        .unwrap_or_else(|_| panic!("read track details"));
}

fn track_detail_query(track_id: char) -> RemoteQuery {
    RemoteQuery {
        query_id: 1,
        kind: RemoteCatalogKind::Track,
        text: track_id.to_string().repeat(22),
        offset: 0,
    }
}

fn assert_album_memo_track(request: &Request) {
    assert_eq!(request.method, "GET");
    assert!(request.authorized);
    let url = reqwest::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
    assert_eq!(url.path(), format!("/v1/tracks/{}", "4".repeat(22)));
}

#[test]
fn resume_uses_exact_album_from_completed_track_details_without_probes() {
    let server = Server::new();
    let (_provider, mut catalog, transport) = album_memo_fixture(&server, &server);
    read_album_memo_track(&mut catalog, '4');

    let control = resume("album", 'm');
    transport
        .control(&control, Some("phone"))
        .unwrap_or_else(|_| panic!("resume a verified album member"));

    let records = server.records();
    let [detail, play] = records.as_slice() else {
        panic!("a verified track detail should be followed directly by Play");
    };
    assert_album_memo_track(detail);
    assert_play(
        play,
        json!({
            "context_uri": format!("spotify:album:{}", "m".repeat(22)),
            "offset": {"uri": format!("spotify:track:{}", "4".repeat(22))},
            "position_ms": POSITION,
        }),
    );
}

#[test]
fn resume_track_album_memo_requires_exact_item_context_and_generation() {
    for (detail_track, saved_album, replace_credential) in [
        ('4', 'f', false),
        ('1', 'm', false),
        // The fixture returns track 4 when requesting track 2.
        ('2', 'm', false),
        ('4', 'm', true),
    ] {
        let server = Server::new();
        let (mut provider, mut catalog, mut transport) = album_memo_fixture(&server, &server);
        read_album_memo_track(&mut catalog, detail_track);
        if replace_credential {
            provider
                .submit(RemoteAction::Credential {
                    access_token: "fixture-bearer".into(),
                })
                .unwrap();
            transport.generation = provider.shared.0.lock().unwrap().generation;
        }

        let control = resume("album", saved_album);
        transport
            .control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("resume via the normal verification path"));

        let records = server.records();
        let [_, album_probe, track_probe, play] = records.as_slice() else {
            panic!("a mismatched memo must leave the two verification probes in place");
        };
        assert_probes(std::slice::from_ref(album_probe), "album", 1);
        assert_album_memo_track(track_probe);
        let track_uri = format!("spotify:track:{}", "4".repeat(22));
        let body = if saved_album == 'm' {
            json!({
                "context_uri": format!("spotify:album:{}", "m".repeat(22)),
                "offset": {"uri": track_uri},
                "position_ms": POSITION,
            })
        } else {
            json!({"uris": [track_uri], "position_ms": POSITION})
        };
        assert_play(play, body);
    }
}

#[test]
fn resume_reuses_track_details_completed_during_the_album_probe() {
    let catalog_server = Server::new();
    let transport_server = Server::new();
    let (_provider, mut catalog, transport) =
        album_memo_fixture(&catalog_server, &transport_server);
    transport_server
        .hold_resume_probe
        .store(true, Ordering::SeqCst);

    thread::scope(|scope| {
        let resume = scope.spawn(move || transport.control(&resume("album", 'm'), Some("phone")));
        let deadline = Instant::now() + Duration::from_secs(1);
        while transport_server.records().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let detail = (!transport_server.records().is_empty())
            .then(|| catalog.catalog(&track_detail_query('4')));
        transport_server
            .hold_resume_probe
            .store(false, Ordering::SeqCst);
        assert!(detail.is_some_and(|result| result.is_ok()));
        resume
            .join()
            .unwrap()
            .unwrap_or_else(|_| panic!("resume with track details from the parallel worker"));
    });

    let catalog_records = catalog_server.records();
    let [detail] = catalog_records.as_slice() else {
        panic!("the catalog worker should only read the track detail once");
    };
    assert_album_memo_track(detail);
    let transport_records = transport_server.records();
    let [album_probe, play] = transport_records.as_slice() else {
        panic!("the transport worker should not repeat the finished track detail read");
    };
    assert_probes(std::slice::from_ref(album_probe), "album", 1);
    assert_play(
        play,
        json!({
            "context_uri": format!("spotify:album:{}", "m".repeat(22)),
            "offset": {"uri": format!("spotify:track:{}", "4".repeat(22))},
            "position_ms": POSITION,
        }),
    );
}

#[test]
fn resume_context_membership_preserves_album_and_playlist_offsets() {
    for (kind, id, page_count, row) in [
        ("playlist", 'a', 1, None),
        ("playlist", 'c', 2, None),
        ("album", 'a', 1, Some(1)),
        ("album", 'c', 2, Some(51)),
        ("album", 'i', 1, Some(1)),
        ("album", 'l', 1, None),
    ] {
        let server = Server::new();
        let api = fixture_api(&server);
        let control = resume(kind, id);
        api.control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("resume a context member"));
        let records = server.records();
        let (play, probes) = records.split_last().unwrap();
        assert_probes(probes, kind, page_count);
        let RemoteControl::ResumeOnDevice {
            item, context_uri, ..
        } = control
        else {
            unreachable!()
        };
        let offset = row.map_or_else(
            || json!({"uri":item.id.as_str()}),
            |position| json!({"position":position}),
        );
        assert_play(
            play,
            json!({"context_uri":context_uri,"offset":offset,"position_ms":POSITION}),
        );
    }
}

#[test]
fn owned_context_resume_projects_before_the_blocked_web_playback_read() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut control = resume("album", 'a');
    if let RemoteControl::ResumeOnDevice { device_id, .. } = &mut control {
        *device_id = "fixture-device".into();
    }
    server
        .hold_positioned_playback
        .store(true, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Control {
            control: control.clone(),
        })
        .unwrap();
    let RemoteUpdate::State(receipt) = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == OPERATION && result.error.is_none())),
    ) else {
        unreachable!()
    };
    assert!(!receipt.owned_output_observed);
    for (sequence, state) in [(1, PlaybackState::Loading), (2, PlaybackState::Playing)] {
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(native_resume_clock(&control, sequence, state)),
            })
            .unwrap();
        let Some(RemoteUpdate::State(output)) = provider.poll() else {
            panic!("missing native projection");
        };
        assert_eq!(
            output.owned_output_observed,
            state == PlaybackState::Playing
        );
        assert!(!output.owned_output_control);
        if state == PlaybackState::Playing {
            assert!(output.observation_id > receipt.observation_id);
            let frame = output.playback.unwrap();
            assert_eq!(
                frame.context_uri.as_deref(),
                Some(format!("spotify:album:{}", "a".repeat(22)).as_str())
            );
            assert_eq!(frame.position_ms, POSITION);
        }
    }
    server
        .hold_positioned_playback
        .store(false, Ordering::SeqCst);
    assert_eq!(
        server
            .records()
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
}

#[test]
fn scoped_resume_pause_confirms_in_both_orders_without_http_replay() {
    for outcome in [
        "ack_first",
        "clock_first",
        "actionless_clock_first",
        "rejected",
        "changed_epoch",
        "changed_while_reserved",
        "changed_before_ack",
        "changed_actionless_before_ack",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut control = resume("album", 'a');
        if let RemoteControl::ResumeOnDevice { device_id, .. } = &mut control {
            *device_id = "fixture-device".into();
        }
        server
            .hold_positioned_playback
            .store(true, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Control {
                control: control.clone(),
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == OPERATION && result.error.is_none())),
        );
        assert!(
            !provider
                .submit_owned_pause_after_resume(OPERATION, "fixture-device", 31)
                .unwrap()
        );
        let observe = |provider: &mut SpotifyRemoteProvider, clock: RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock { clock: Some(clock) })
                .unwrap();
            let Some(RemoteUpdate::State(state)) = provider.poll() else {
                panic!("missing output state");
            };
            state
        };
        observe(
            &mut provider,
            native_resume_clock(&control, 1, PlaybackState::Loading),
        );
        let playing = observe(
            &mut provider,
            native_resume_clock(&control, 2, PlaybackState::Playing),
        );
        assert!(playing.owned_output_observed && !playing.owned_output_control);

        assert!(
            provider
                .submit_owned_pause_after_resume(OPERATION + 1, "fixture-device", 32)
                .is_err()
        );
        assert!(
            provider
                .submit_owned_pause_after_resume(OPERATION, "another-device", 32)
                .is_err()
        );
        if outcome == "changed_epoch" {
            let mut changed = native_resume_clock(&control, 3, PlaybackState::Playing);
            changed.queue_epoch = Some(8);
            observe(&mut provider, changed);
            assert!(
                provider
                    .submit_owned_pause_after_resume(OPERATION, "fixture-device", 32)
                    .is_err()
            );
        } else {
            assert!(
                provider
                    .submit_owned_pause_after_resume(OPERATION, "fixture-device", 32)
                    .unwrap()
            );
            assert!(provider.owned_play_pause_ready_for_request(32));
            assert!(!provider.owned_play_pause_ready_for_request(31));
            if outcome == "changed_while_reserved" {
                let mut changed = native_resume_clock(&control, 3, PlaybackState::Playing);
                changed.queue_epoch = Some(8);
                observe(&mut provider, changed);
            }
            let RemoteUpdate::OutputCommand(command) = wait(&mut provider, |update| {
                if outcome == "changed_while_reserved" {
                    return match update {
                        RemoteUpdate::OutputCommand(_) => {
                            panic!("stale pause escaped its reservation")
                        }
                        RemoteUpdate::OutputCommandFailed {
                            control: RemoteOutputControl::Pause,
                            ..
                        } => true,
                        _ => false,
                    };
                }
                matches!(update, RemoteUpdate::OutputCommand(_))
            }) else {
                assert_eq!(outcome, "changed_while_reserved");
                assert!(server.records().iter().all(|request| request.method != "PUT" || request.target.contains("/play?")));
                server
                    .hold_positioned_playback
                    .store(false, Ordering::SeqCst);
                continue;
            };
            assert_eq!(command.control, RemoteOutputControl::Pause);
            assert_eq!(command.required_queue_epoch, Some(7));
            assert_eq!(command.request_id, Some(32));
            assert_eq!(command.sequence, 2);
            assert!(!provider.owned_play_pause_ready_for_request(32));
            assert!(
                provider
                    .submit_owned_pause_after_resume(OPERATION, "fixture-device", 33)
                    .is_err()
            );

            let acknowledge = |provider: &mut SpotifyRemoteProvider, accepted| {
                provider
                    .submit(RemoteAction::OutputCommandResult {
                        result: RemoteOutputCommandResult {
                            id: command.id,
                            generation: command.generation,
                            accepted,
                        },
                    })
                    .unwrap();
            };
            if outcome == "actionless_clock_first" {
                let mut partial = native_resume_clock(&control, 3, PlaybackState::Paused);
                partial.actions = None;
                partial.position_ms = POSITION + 400;
                let provisional = observe(&mut provider, partial.clone());
                assert!(!provisional.owned_output_observed && !provisional.owned_output_control);
                assert_eq!(provisional.observation_id, playing.observation_id);
                let frame = provisional
                    .playback
                    .expect("exact selection remains visible");
                assert_eq!(
                    frame.current.unwrap().id,
                    playing.playback.unwrap().current.unwrap().id
                );
                assert_eq!(frame.state, PlaybackState::Loading);
                assert_eq!(frame.position_ms, POSITION);
                assert_eq!(frame.position_rate, 0.0);
                assert_eq!(frame.capabilities, Capabilities::default());
                assert_eq!(frame.transport, TransportControls::default());

                let complete = observe(
                    &mut provider,
                    native_resume_clock(&control, 4, PlaybackState::Paused),
                );
                assert!(complete.owned_output_observed && !complete.owned_output_control);
                assert!(complete.observation_id > provisional.observation_id);
                assert_eq!(complete.playback.unwrap().state, PlaybackState::Paused);

                partial.sequence = 5;
                partial.control_uptime_ms = Some(500);
                let retained = observe(&mut provider, partial);
                assert!(retained.owned_output_observed && !retained.owned_output_control);
                assert_eq!(retained.observation_id, complete.observation_id);
                let retained = retained.playback.unwrap();
                assert_eq!(retained.state, PlaybackState::Paused);
                assert_eq!(retained.position_ms, POSITION);
                assert_eq!(retained.capabilities, Capabilities::default());
            }
            let changed_before_ack = matches!(
                outcome,
                "changed_before_ack" | "changed_actionless_before_ack"
            );
            if outcome == "clock_first" || changed_before_ack {
                let mut clock = native_resume_clock(&control, 3, PlaybackState::Paused);
                if changed_before_ack {
                    clock.queue_epoch = Some(8);
                }
                if outcome == "changed_actionless_before_ack" {
                    clock.actions = None;
                }
                let before = observe(&mut provider, clock);
                if outcome == "clock_first" {
                    assert!(before.owned_output_observed && !before.owned_output_control);
                    let frame = before
                        .playback
                        .expect("the confirmed selection stays visible before ACK");
                    assert_eq!(frame.state, PlaybackState::Paused);
                    assert_eq!(frame.position_ms, POSITION);
                } else {
                    assert!(!before.owned_output_observed && !before.owned_output_control);
                    assert!(
                        before
                            .playback
                            .is_none_or(|frame| frame.state != PlaybackState::Loading)
                    );
                }
            }
            acknowledge(&mut provider, outcome != "rejected");
            if outcome == "ack_first" {
                let before = provider.poll();
                assert!(
                    !matches!(before, Some(RemoteUpdate::State(state)) if state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused && state.owned_output_observed))
                );
                provider
                    .submit(RemoteAction::OutputClock {
                        clock: Some(native_resume_clock(&control, 3, PlaybackState::Paused)),
                    })
                    .unwrap();
            }
            if changed_before_ack {
                while let Some(update) = provider.poll() {
                    assert!(
                        !matches!(update, RemoteUpdate::State(state) if state.owned_output_observed && state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused))
                    );
                }
            } else if outcome == "rejected" {
                wait(&mut provider, |update| {
                    matches!(
                        update,
                        RemoteUpdate::OutputCommandFailed {
                            control: RemoteOutputControl::Pause,
                            ..
                        }
                    )
                });
                assert!(
                    provider
                        .submit_owned_pause_after_resume(OPERATION, "fixture-device", 33)
                        .is_err()
                );
            } else {
                let RemoteUpdate::State(paused) = wait(
                    &mut provider,
                    |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_observed && state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused)),
                ) else {
                    unreachable!()
                };
                assert!(!paused.owned_output_control);
                assert_eq!(paused.playback.unwrap().position_ms, POSITION);
            }
        }
        assert!(
            server
                .records()
                .iter()
                .all(|request| request.method != "PUT" || request.target.contains("/play?"))
        );
        server
            .hold_positioned_playback
            .store(false, Ordering::SeqCst);
    }
}

#[test]
fn native_resume_proof_rejects_stale_and_unrelated_observations() {
    for case in [
        "wrong_context",
        "new_generation",
        "old_sequence",
        "old_uptime",
        "wrong_device",
        "off_position",
        "before_dispatch",
        "without_loading",
        "wrong_receipt",
        "external_device",
        "inactive_origin",
        "late_ack",
    ] {
        let control = resume("album", 'a');
        let now = Instant::now();
        let dispatch = now + Duration::from_millis(10);
        let mut projection = output_clock::OutputProjection::default();
        let mut base = output_base(1);
        base.playback = None;
        base.devices = vec![mapping::device(&frame()["device"]).unwrap()];
        base.devices[0].id = "owned-device".into();
        base.devices[0].is_active = false;
        projection.ingest(base.clone(), now);
        if case == "inactive_origin" {
            let mut prior = native_resume_clock(&control, 1, PlaybackState::Paused);
            prior.active = false;
            projection.observe(Some(prior), now).unwrap();
        }
        assert!(projection.remember_http_selection(&control, now).is_none());

        let load_at = if case == "before_dispatch" {
            now
        } else {
            now + Duration::from_millis(20)
        };
        if case != "without_loading" {
            projection
                .observe(
                    Some(native_resume_clock(&control, 2, PlaybackState::Loading)),
                    load_at,
                )
                .unwrap();
        }
        let mut playing = native_resume_clock(&control, 3, PlaybackState::Playing);
        match case {
            "wrong_context" => {
                playing.metadata.as_mut().unwrap().context_uri =
                    Some(format!("spotify:album:{}", "b".repeat(22)))
            }
            "new_generation" => playing.generation += 1,
            "old_sequence" => playing.sequence = 2,
            "old_uptime" => playing.control_uptime_ms = Some(50),
            "wrong_device" => playing.device_id = "another-device".into(),
            "off_position" => playing.position_ms = 0,
            _ => {}
        }
        let played_at = now + Duration::from_millis(30);
        projection.observe(Some(playing), played_at).unwrap();
        assert!(
            !projection.take(played_at).unwrap().owned_output_observed,
            "pre-ACK {case}"
        );
        projection.accept_context_resume(output_clock::ContextResumeReceipt {
            operation: if case == "wrong_receipt" {
                OPERATION + 1
            } else {
                OPERATION
            },
            dispatched_at: dispatch,
        });
        if case == "external_device" {
            base.devices
                .push(mapping::device(&frame()["device"]).unwrap());
        }
        projection.ingest(base, played_at);
        let output = projection.take(played_at).unwrap();
        let expected = matches!(case, "inactive_origin" | "late_ack");
        assert_eq!(output.owned_output_observed, expected, "{case}");
        assert!(!output.owned_output_control, "{case}");
        if expected {
            assert_eq!(output.playback.unwrap().position_ms, POSITION);
        }
    }
}

#[test]
fn resume_keeps_the_tracks_verified_album_when_its_rows_cannot_be_read() {
    for (id, verified) in [('m', true), ('f', false)] {
        let server = Server::new();
        let api = fixture_api(&server);
        let control = resume("album", id);
        api.control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("resume the exact item"));
        let records = server.records();
        assert_eq!(records.len(), 3);
        assert_probes(&records[..1], "album", 1);
        let RemoteControl::ResumeOnDevice {
            item, context_uri, ..
        } = control
        else {
            unreachable!()
        };
        let track = item.id.as_str();
        assert_eq!(records[1].method, "GET");
        let url = reqwest::Url::parse(&format!("http://fixture{}", records[1].target)).unwrap();
        assert_eq!(
            url.path(),
            format!(
                "/v1/tracks/{}",
                track.strip_prefix("spotify:track:").unwrap()
            )
        );
        let body = if verified {
            json!({"context_uri":context_uri,"offset":{"uri":track},"position_ms":POSITION})
        } else {
            json!({"uris":[track],"position_ms":POSITION})
        };
        assert_play(&records[2], body);
    }
}

#[test]
fn rejected_album_resume_never_replays_as_a_standalone_track() {
    let server = Server::new();
    server.mode.store(292, Ordering::SeqCst);
    let api = fixture_api(&server);
    let control = resume("album", 'a');
    let error = api
        .control(&control, Some("phone"))
        .expect_err("fixture HTTP rejection");
    assert_eq!(error.status, 404);
    let records = server.records();
    assert_eq!(records.len(), 2);
    assert_probes(&records[..1], "album", 1);
    let RemoteControl::ResumeOnDevice { context_uri, .. } = control else {
        unreachable!()
    };
    assert_play(
        &records[1],
        json!({"context_uri":context_uri,"offset":{"position":1},"position_ms":POSITION}),
    );
}

#[test]
fn resume_known_nonmember_does_not_guess_an_album_offset() {
    let server = Server::new();
    let api = fixture_api(&server);
    let control = resume("album", 'b');
    api.control(&control, Some("phone"))
        .unwrap_or_else(|_| panic!("resume the exact item"));
    let records = server.records();
    assert_eq!(records.len(), 2);
    assert_probes(&records[..1], "album", 1);
    let RemoteControl::ResumeOnDevice { item, .. } = control else {
        unreachable!()
    };
    assert_play(
        &records[1],
        json!({"uris":[item.id.as_str()],"position_ms":POSITION}),
    );
}

#[test]
fn resume_without_proven_context_membership_plays_the_exact_item_once() {
    // A queue/Autoplay track can retain the originating playlist's context.
    // Also cover a match beyond the probe bound, denied reads and malformed rows.
    for (id, page_count) in [('b', 1), ('d', 2), ('e', 1), ('f', 1), ('m', 1)] {
        let server = Server::new();
        let api = fixture_api(&server);
        let control = resume("playlist", id);
        api.control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("resume the exact item"));
        let records = server.records();
        let (play, probes) = records.split_last().unwrap();
        assert_probes(probes, "playlist", page_count);
        let RemoteControl::ResumeOnDevice { item, .. } = control else {
            unreachable!()
        };
        assert_play(
            play,
            json!({"uris":[item.id.as_str()],"position_ms":POSITION}),
        );
    }
}

#[test]
fn resume_context_authentication_or_quota_failure_prevents_play() {
    for status in [401, 429] {
        let server = Server::new();
        server.mode.store(status, Ordering::SeqCst);
        let api = fixture_api(&server);
        let failure = api
            .control(&resume("playlist", 'a'), Some("phone"))
            .err()
            .unwrap();
        assert_eq!(failure.status, status);
        assert_probes(&server.records(), "playlist", 1);
    }
}

fn wait_for_resume_fixture(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for resume fixture"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn exact_resume_cancellation_removes_a_queued_or_preflight_write_without_pause() {
    for in_preflight in [false, true] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        if in_preflight {
            server.hold_resume_probe.store(true, Ordering::SeqCst);
        } else {
            let playback_reads = || {
                server
                    .records()
                    .iter()
                    .filter(|request| {
                        request.method == "GET"
                            && request.target.split('?').next() == Some("/v1/me/player")
                    })
                    .count()
            };
            let prior_reads = playback_reads();
            server.hold_playback.store(true, Ordering::SeqCst);
            provider.submit(RemoteAction::Refresh).unwrap();
            wait_for_resume_fixture(|| playback_reads() > prior_reads);
        }
        provider
            .submit(RemoteAction::Control {
                control: resume("album", 'a'),
            })
            .unwrap();
        if in_preflight {
            wait_for_resume_fixture(|| {
                server
                    .records()
                    .iter()
                    .any(|request| request.method == "GET" && request.target.contains("/tracks?"))
            });
        }
        let id_mismatch = provider.cancel_resume_before_dispatch(OPERATION + 1, "owned-device");
        let device_mismatch = provider.cancel_resume_before_dispatch(OPERATION, "another-device");
        let cancelled = provider.cancel_resume_before_dispatch(OPERATION, "owned-device");
        server.hold_resume_probe.store(false, Ordering::SeqCst);
        server.hold_playback.store(false, Ordering::SeqCst);
        assert!(!id_mismatch && !device_mismatch && cancelled);
        wait_for_resume_fixture(|| {
            let mailbox = provider.shared.0.lock().unwrap();
            mailbox.resume_dispatch.is_none() && mailbox.controls.is_empty()
        });
        while let Some(update) = provider.poll() {
            if let RemoteUpdate::State(state) = update {
                assert!(
                    state
                        .control_result
                        .as_ref()
                        .is_none_or(|result| result.operation_id != OPERATION)
                );
            }
        }
        assert!(
            server
                .records()
                .iter()
                .all(|request| request.method != "PUT")
        );
    }
}

#[test]
fn targeted_resume_cannot_be_cancelled_after_put_even_before_its_response() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.hold_play_response.store(true, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Control {
            control: resume("album", 'a'),
        })
        .unwrap();
    wait_for_resume_fixture(|| {
        server.records().iter().any(|request| {
            request.method == "PUT" && request.target == "/v1/me/player/play?device_id=owned-device"
        })
    });
    let cancelled = provider.cancel_resume_before_dispatch(OPERATION, "owned-device");
    let pause = provider.submit(RemoteAction::Control {
        control: RemoteControl::PauseOnDevice {
            device_id: "owned-device".into(),
        },
    });
    server.hold_play_response.store(false, Ordering::SeqCst);
    assert!(!cancelled);
    pause.unwrap();
    wait_for_resume_fixture(|| {
        server.records().iter().any(|request| {
            request.method == "PUT"
                && request.target == "/v1/me/player/pause?device_id=owned-device"
        })
    });
    let writes = server
        .records()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 2);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/play?device_id=owned-device"
    );
    assert_eq!(
        writes[1].target,
        "/v1/me/player/pause?device_id=owned-device"
    );
}

#[test]
fn pause_while_resume_context_probe_is_in_flight_cancels_play_without_backoff() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.hold_resume_probe.store(true, Ordering::SeqCst);
    let control = resume("playlist", 'a');
    assert!(supersedes(
        &RemoteControl::Transfer {
            device_id: "phone".into(),
            operation_id: None,
        },
        &control
    ));
    provider.submit(RemoteAction::Control { control }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !server
        .records()
        .iter()
        .any(|request| request.target.contains("/items?"))
    {
        assert!(
            Instant::now() < deadline,
            "Resume membership probe did not start"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let pause = provider.submit(RemoteAction::Control {
        control: RemoteControl::PauseOnDevice {
            device_id: "owned-device".into(),
        },
    });
    server.hold_resume_probe.store(false, Ordering::SeqCst);
    pause.unwrap();
    let state = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
            if state.control_result.as_ref().is_some_and(|result| result.operation_id == OPERATION && result.error.is_some())
                && state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused))
    });
    let RemoteUpdate::State(state) = state else {
        unreachable!()
    };
    assert_eq!(state.connection, ProviderConnectionState::Connected);
    assert!(state.issue.is_none());
    let writes = server
        .records()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/pause?device_id=owned-device"
    );
}
