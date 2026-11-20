use super::*;

fn context(kind: &str) -> String {
    format!("spotify:{kind}:{}", "a".repeat(22))
}

fn start(uri: String, device: &str, item: MediaItem, operation: u64) -> RemoteControl {
    RemoteControl::StartContext {
        uri,
        device_id: device.into(),
        item: Some(item),
        operation_id: Some(operation),
        position: None,
        query_id: None,
    }
}

#[test]
fn duplicate_playlist_row_uses_its_absolute_position_across_the_two_workers() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let id = "s".repeat(22);
    let uri = format!("spotify:playlist:{id}");
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 70,
                kind: RemoteCatalogKind::PlaylistItems,
                text: id,
                offset: 10,
            },
        })
        .unwrap();
    let RemoteUpdate::Page(page) = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::Page(page) if page.query_id == 70),
    ) else {
        unreachable!();
    };
    assert_eq!(page.item_positions, [10, 12]);
    assert_eq!(page.items[0].id, page.items[1].id);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartContext {
                uri: uri.clone(),
                device_id: "fixture-device".into(),
                item: Some(page.items[1].clone()),
                operation_id: Some(27),
                position: Some(12),
                query_id: Some(70),
            },
        })
        .unwrap();
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
            if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_none())
                && state.playback.as_ref().is_some_and(|frame| frame.context_uri.as_ref() == Some(&uri)
                    && frame.current.as_ref().is_some_and(|current| current.id == page.items[1].id)))
    });
    let requests = server.records();
    let writes: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/play?device_id=fixture-device"
    );
    assert_eq!(
        writes[0].body,
        json!({"context_uri":uri,"offset":{"position":12},"position_ms":0})
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.target.contains("fields=snapshot_id"))
            .count(),
        2
    );
}

#[test]
fn invalidated_context_rows_cannot_write_a_different_song_or_collection() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let page = move_page(&mut api, 70);
    let uri = format!("spotify:playlist:{}", "s".repeat(22));
    let row = |position, query_id, item| RemoteControl::StartContext {
        uri: uri.clone(),
        device_id: "fixture-device".into(),
        item: Some(item),
        operation_id: Some(27),
        position: Some(position),
        query_id: Some(query_id),
    };
    let wrong_item = mapping::track(&track('2')).unwrap();
    let item = page.items[1].clone();
    let before = server.records().len();
    for stale in [
        row(11, 70, item.clone()),
        row(12, 69, item.clone()),
        row(12, 70, wrong_item),
    ] {
        let issue = api.control(&stale, None).err().unwrap();
        assert_eq!(issue.error.code, ErrorCode::CatalogChanged);
        assert_eq!(server.records().len(), before);
    }
    server.playlist_version.store(2, Ordering::SeqCst);
    let stale = api.control(&row(12, 70, item), None).err().unwrap();
    assert_eq!(stale.error.code, ErrorCode::CatalogChanged);
    assert_eq!(server.records().len(), before + 1);
    assert!(!server.records().iter().any(|r| r.method == "PUT"));
}

#[test]
fn missing_playlist_version_uses_a_fresh_read_of_the_exact_row() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    server.playlist_version.store(0, Ordering::SeqCst);
    let page = move_page(&mut api, 70);
    let id = "s".repeat(22);
    let uri = format!("spotify:playlist:{id}");
    api.control(
        &RemoteControl::StartContext {
            uri: uri.clone(),
            device_id: "fixture-device".into(),
            item: Some(page.items[1].clone()),
            operation_id: Some(27),
            position: Some(12),
            query_id: Some(70),
        },
        None,
    )
    .unwrap_or_else(|_| panic!("current exact row"));
    let requests = server.records();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .target
            .starts_with(&format!("/v1/playlists/{id}/items?limit=1&offset=12&"))
    );
    assert_eq!(
        requests[3].body,
        json!({"context_uri":uri,"offset":{"position":12},"position_ms":0})
    );
}

#[test]
fn legacy_context_wire_stays_compatible_and_item_offset_uses_one_targeted_http_play() {
    let server = Server::new();
    let api = fixture_api(&server);
    let item = mapping::track(&track('2')).unwrap();
    let legacy: RemoteControl = serde_json::from_value(json!({
        "kind":"start_context", "uri":context("artist"), "device_id":"fixture-device"
    }))
    .unwrap();
    assert!(matches!(
        legacy,
        RemoteControl::StartContext { item: None, .. }
    ));
    assert!(serde_json::to_value(legacy).unwrap().get("item").is_none());
    for kind in ["album", "playlist"] {
        let uri = context(kind);
        let control: RemoteControl = serde_json::from_value(json!({
            "kind":"start_context", "uri":uri, "device_id":"fixture-device",
            "item":item, "operation_id":27
        }))
        .unwrap();
        assert_eq!(control.operation_id(), Some(27));
        assert!(owned_commands::convert(&control, Some(&output_clock(1))).is_none());
        api.control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("targeted context start"));
    }
    let records = server.records();
    assert_eq!(records.len(), 2);
    for (record, kind) in records.iter().zip(["album", "playlist"]) {
        assert_eq!(record.method, "PUT");
        assert_eq!(record.target, "/v1/me/player/play?device_id=fixture-device");
        assert_eq!(
            record.body,
            json!({
                "context_uri":context(kind), "offset":{"uri":item.id.as_str()}, "position_ms":0
            })
        );
    }
}

#[test]
fn invalid_context_item_starts_are_rejected_before_http() {
    let server = Server::new();
    let api = fixture_api(&server);
    let mut provider = server.provider();
    let item = mapping::track(&track('2')).unwrap();
    let mut unavailable = item.clone();
    unavailable.capabilities.play = false;
    let mut wrong_location = item.clone();
    wrong_location.location = mapping::track(&track('3')).unwrap().location;
    let episode = mapping::track(&spoken_fixture("episode")).unwrap();
    let mut foreign = item.clone();
    foreign.id = MediaId::parse("fixture:track:original").unwrap();
    let valid = context("album");
    for control in [
        start(context("artist"), "fixture-device", item.clone(), 1),
        start(context("audiobook"), "fixture-device", item.clone(), 1),
        start(
            "spotify:album:malformed".into(),
            "fixture-device",
            item.clone(),
            1,
        ),
        start(valid.clone(), "bad device", item.clone(), 1),
        start(valid.clone(), "fixture-device", item, 0),
        start(valid.clone(), "fixture-device", unavailable, 1),
        start(valid.clone(), "fixture-device", wrong_location, 1),
        start(valid.clone(), "fixture-device", episode, 1),
        start(valid, "fixture-device", foreign, 1),
    ] {
        assert_eq!(api.control(&control, None).err().unwrap().status, 400);
        assert!(provider.submit(RemoteAction::Control { control }).is_err());
    }
    assert!(server.records().is_empty());
}

#[test]
fn selection_supersession_and_confirmation_require_the_requested_context_item_and_device() {
    let server = Server::new();
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared.clone()).unwrap();
    api.token = Some("fixture-bearer".into());
    let item = mapping::track(&track('2')).unwrap();
    let uri = context("playlist");
    let control = start(uri.clone(), "fixture-device", item.clone(), 27);
    let pause = |device: &str| RemoteControl::PauseOnDevice {
        device_id: device.into(),
    };
    assert!(!supersedes(&pause("phone"), &control));
    for replacement in [
        pause("fixture-device"),
        RemoteControl::Pause,
        RemoteControl::Transfer {
            device_id: "phone".into(),
            operation_id: None,
        },
        start(context("album"), "fixture-device", item, 28),
    ] {
        assert!(supersedes(&replacement, &control));
        shared
            .0
            .lock()
            .unwrap()
            .controls
            .push_back(replacement.into());
        assert_eq!(api.control(&control, None).err().unwrap().status, 499);
        shared.0.lock().unwrap().controls.clear();
    }
    assert!(server.records().is_empty());

    let confirmation = ControlConfirmation {
        control,
        previous: None,
        until: Instant::now(),
        attempts: 0,
    };
    let mut value = frame();
    value["context"] = json!({"uri":uri});
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["item"] = track('2');
    value["item"]["duration_ms"] = json!(180_000);
    value["context"] = json!({"uri":context("album")});
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["context"] = json!({"uri":uri});
    value["device"]["id"] = json!("phone");
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["device"]["id"] = json!("fixture-device");
    value["device"]["is_active"] = json!(false);
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["device"]["is_active"] = json!(true);
    value["device"]["is_restricted"] = json!(true);
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["device"]["is_restricted"] = json!(false);
    value["progress_ms"] = json!(20_000);
    assert!(!confirmation.matches(Some(&mapping::playback(&value))));
    value["progress_ms"] = json!(1100);
    assert!(confirmation.matches(Some(&mapping::playback(&value))));
}

#[test]
fn context_item_play_reports_the_selected_item_context_and_device() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let item = mapping::track(&track('2')).unwrap();
    let uri = context("playlist");
    provider
        .submit(RemoteAction::Control {
            control: start(uri.clone(), "explicit-output", item.clone(), 27),
        })
        .unwrap();
    let update = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_none())
            && state.playback.as_ref().is_some_and(|frame| frame.current.as_ref().is_some_and(|current| current.id == item.id)
                && frame.context_uri.as_deref() == Some(&uri)
                && frame.device.as_ref().is_some_and(|device| device.id == "explicit-output")))
    });
    assert!(matches!(update, RemoteUpdate::State(_)));
    let writes: Vec<_> = server
        .records()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/play?device_id=explicit-output"
    );
}

#[test]
fn tagged_http_play_publishes_acceptance_before_the_playback_read_returns() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let previous = mapping::track(&track('1')).unwrap();
    let item = mapping::track(&track('2')).unwrap();
    let uri = context("playlist");
    server.hold_playback.store(true, Ordering::SeqCst);
    let started = Instant::now();
    provider
        .submit(RemoteAction::Control {
            control: start(uri.clone(), "fixture-device", item.clone(), 27),
        })
        .unwrap();
    let RemoteUpdate::State(receipt) = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_none()))
    }) else {
        unreachable!()
    };
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(receipt.issue.is_none());
    let observed = receipt.playback.unwrap();
    assert_eq!(observed.current.unwrap().id, previous.id);
    assert_eq!(observed.context_uri, None);
    assert!(provider.has_pending_output_start());
    assert!(provider.shared.0.lock().unwrap().http_mutating);

    server.hold_playback.store(false, Ordering::SeqCst);
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.observation_id > receipt.observation_id
            && state.playback.as_ref().is_some_and(|frame| frame.context_uri.as_ref() == Some(&uri)
                && frame.current.as_ref().is_some_and(|current| current.id == item.id)))
    });
    assert_eq!(
        server
            .records()
            .iter()
            .filter(|record| record.method == "PUT")
            .count(),
        1
    );
}

#[test]
fn a_newer_positioned_row_on_the_same_device_runs_before_the_obsolete_playback_read() {
    for second_device in ["fixture-device", "another-device"] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let id = "s".repeat(22);
        let uri = format!("spotify:playlist:{id}");
        let query_id = 70;
        provider
            .submit(RemoteAction::Query {
                query: RemoteQuery {
                    query_id,
                    kind: RemoteCatalogKind::PlaylistItems,
                    text: id,
                    offset: 10,
                },
            })
            .unwrap();
        let RemoteUpdate::Page(page) = wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::Page(page) if page.query_id == query_id),
        ) else {
            unreachable!()
        };
        let select = |index: usize, device: &str, operation| RemoteAction::Control {
            control: RemoteControl::StartContext {
                uri: uri.clone(),
                device_id: device.into(),
                item: Some(page.items[index].clone()),
                operation_id: Some(operation),
                position: Some(page.item_positions[index]),
                query_id: Some(query_id),
            },
        };
        let is_play = |request: &Request| {
            request.method == "PUT" && request.target.starts_with("/v1/me/player/play?")
        };
        server.hold_play_response.store(true, Ordering::SeqCst);
        let first = provider.submit(select(0, "fixture-device", 27));
        let deadline = Instant::now() + Duration::from_secs(2);
        while first.is_ok() && !server.records().iter().any(is_play) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let first_dispatched = server.records().iter().any(is_play);
        let second = first_dispatched.then(|| provider.submit(select(1, second_device, 28)));
        server.hold_play_response.store(false, Ordering::SeqCst);
        first.unwrap();
        assert!(first_dispatched);
        second.unwrap().unwrap();
        wait(&mut provider, |update| {
            matches!(update, RemoteUpdate::State(state)
                if state.control_result.as_ref().is_some_and(|result| result.operation_id == 28 && result.error.is_none())
                    && state.playback.as_ref().is_some_and(|frame| frame.context_uri.as_ref() == Some(&uri)
                        && frame.current.as_ref().is_some_and(|item| item.id == page.items[1].id)
                        && frame.device.as_ref().is_some_and(|device| device.id == second_device)))
        });
        let requests = server.records();
        let first_write = requests.iter().position(is_play).unwrap();
        let transport: Vec<_> = requests[first_write..]
            .iter()
            .filter(|request| {
                is_play(request)
                    || request.method == "GET" && request.target.starts_with("/v1/me/player?")
            })
            .collect();
        let write_indices: Vec<_> = transport
            .iter()
            .enumerate()
            .filter_map(|(index, request)| is_play(request).then_some(index))
            .collect();
        assert_eq!(
            write_indices,
            [
                0,
                if second_device == "fixture-device" {
                    1
                } else {
                    2
                }
            ]
        );
        let latest = transport[write_indices[1]];
        assert_eq!(transport[0].body["offset"]["position"], 10);
        assert_eq!(latest.body["offset"]["position"], 12);
        assert_eq!(
            latest.target,
            format!("/v1/me/player/play?device_id={second_device}")
        );
        assert_eq!(transport[write_indices[1] + 1].method, "GET");
    }
}

#[test]
fn positioned_album_row_projects_only_the_acknowledged_exact_owned_successor_before_http() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let uri = context("album");
    let query_id = 70;
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id,
                kind: RemoteCatalogKind::AlbumTracks,
                text: "a".repeat(22),
                offset: 0,
            },
        })
        .unwrap();
    let RemoteUpdate::Page(page) = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::Page(page) if page.query_id == query_id),
    ) else {
        unreachable!()
    };
    let item = page.items[0].clone();
    let position = page.item_positions[0];
    let previous = mapping::track(&track('1')).unwrap();
    let clock = |sequence, item: &MediaItem, uri: &str, state, active, position_ms| {
        let mut clock = output_clock(sequence);
        clock.active = active;
        clock.state = state;
        clock.rate = if state == PlaybackState::Playing {
            1.0
        } else {
            0.0
        };
        clock.position_ms = position_ms;
        clock.control_uptime_ms = Some(sequence * 100);
        clock.media_id = Some(item.id.clone());
        clock.metadata = Some(RemoteOutputMetadata {
            title: item.title.clone(),
            artists: item.artists.clone(),
            album: None,
            duration_ms: item.duration_ms,
            artwork: None,
            context_uri: Some(uri.into()),
        });
        clock
    };
    let observe = |provider: &mut SpotifyRemoteProvider, clock| {
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        let Some(RemoteUpdate::State(state)) = provider.poll() else {
            panic!("owned clock projection")
        };
        state
    };
    let initial = observe(
        &mut provider,
        clock(
            1,
            &previous,
            &context("playlist"),
            PlaybackState::Paused,
            false,
            2_000,
        ),
    );
    assert!(!initial.owned_output_observed);

    // The inactive clock above can request an immediate read of the old
    // frame. Hold only a read made after the fixture accepted the row write.
    server
        .hold_positioned_playback
        .store(true, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartContext {
                uri: uri.clone(),
                device_id: "fixture-device".into(),
                item: Some(item.clone()),
                operation_id: Some(27),
                position: Some(position),
                query_id: Some(query_id),
            },
        })
        .unwrap();
    let RemoteUpdate::State(receipt) = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_none())),
    ) else {
        unreachable!()
    };

    for (sequence, item, context_uri, state) in [
        (2, &previous, uri.as_str(), PlaybackState::Playing),
        (
            3,
            &item,
            "spotify:album:bbbbbbbbbbbbbbbbbbbbbb",
            PlaybackState::Playing,
        ),
        (4, &item, uri.as_str(), PlaybackState::Loading),
    ] {
        let observation = observe(
            &mut provider,
            clock(sequence, item, context_uri, state, true, 1),
        );
        assert_ne!(
            observation.playback.unwrap().context_uri.as_deref(),
            Some(uri.as_str())
        );
    }
    let projection = observe(
        &mut provider,
        clock(5, &item, &uri, PlaybackState::Playing, true, 1),
    );
    assert!(projection.observation_id > receipt.observation_id);
    assert!(projection.owned_output_observed && !projection.owned_output_control);
    assert!(provider.output.navigation_clock(Instant::now()).is_none());
    assert!(
        provider
            .output
            .http_navigation_clock(Instant::now())
            .is_some()
    );
    let frame = projection.playback.unwrap();
    assert_eq!(frame.current.unwrap().id, item.id);
    assert_eq!(frame.context_uri.as_deref(), Some(uri.as_str()));
    assert_eq!(frame.position_ms, 1);
    assert!(frame.upcoming.is_empty());
    assert!(provider.has_pending_output_start());

    let writes: Vec<_> = server
        .records()
        .into_iter()
        .filter(|record| record.method == "PUT")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].target,
        "/v1/me/player/play?device_id=fixture-device"
    );
    assert_eq!(
        writes[0].body,
        json!({"context_uri":uri,"offset":{"position":position},"position_ms":0})
    );
    server
        .hold_positioned_playback
        .store(false, Ordering::SeqCst);
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
    );
}

#[test]
fn rejected_context_item_starts_are_correlated_and_never_replayed() {
    for mode in [291, 289] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let item = mapping::track(&track('2')).unwrap();
        server.mode.store(mode, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Control {
                control: start(context("playlist"), "fixture-device", item, 27),
            })
            .unwrap();
        wait(&mut provider, |update| {
            matches!(update, RemoteUpdate::State(state)
            if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_some()))
        });
        server.mode.store(200, Ordering::SeqCst);
        provider.submit(RemoteAction::Refresh).unwrap();
        thread::sleep(Duration::from_millis(1200));
        let writes: Vec<_> = server
            .records()
            .into_iter()
            .filter(|request| request.method == "PUT")
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].target,
            "/v1/me/player/play?device_id=fixture-device"
        );
    }
}
