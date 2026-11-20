use super::*;

const POSITION: u64 = 42_000;

fn start(item: MediaItem, device_id: &str, position_ms: u64, operation_id: u64) -> RemoteControl {
    RemoteControl::StartSpokenOnDevice {
        item,
        device_id: device_id.into(),
        position_ms,
        operation_id,
    }
}

#[test]
fn a_spoken_start_targets_one_play_without_transferring_other_playback() {
    let server = Server::new();
    let api = fixture_api(&server);
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let episode = mapping::track(&spoken_fixture("episode")).unwrap();
    let mut clock = output_clock(1);
    clock.device_id = "owned-output".into();
    for (item, position_ms) in [(chapter, POSITION), (episode.clone(), 1)] {
        let control = start(item, "owned-output", position_ms, 27);
        assert_eq!(control.operation_id(), Some(27));
        assert!(owned_commands::convert(&control, Some(&clock)).is_none());
        api.control(&control, Some("phone"))
            .unwrap_or_else(|_| panic!("positioned start"));
    }
    let records = server.records();
    assert_eq!(records.len(), 2);
    for (record, position_ms) in records.iter().zip([POSITION, 1]) {
        assert_eq!(record.method, "PUT");
        assert_eq!(record.target, "/v1/me/player/play?device_id=owned-output");
        assert_eq!(
            record.body,
            json!({"uris":[episode.id.as_str()],"position_ms":position_ms})
        );
    }
}

#[test]
fn invalid_spoken_starts_never_send_http() {
    let server = Server::new();
    let api = fixture_api(&server);
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let song = mapping::track(&track('1')).unwrap();
    let mut wrong_location = chapter.clone();
    wrong_location.location = song.location.clone();
    let mut wrong_provider = chapter.clone();
    wrong_provider.id = MediaId::parse("fixture:chapter:original").unwrap();
    let mut unavailable = chapter.clone();
    unavailable.capabilities.play = false;
    for control in [
        start(chapter.clone(), "owned-output", 0, 27),
        start(chapter.clone(), "owned-output", chapter.duration_ms, 27),
        start(chapter.clone(), "owned-output", chapter.duration_ms + 1, 27),
        start(chapter.clone(), "bad device", POSITION, 27),
        start(chapter, "owned-output", POSITION, 0),
        start(song, "owned-output", 1000, 27),
        start(wrong_location, "owned-output", POSITION, 27),
        start(wrong_provider, "owned-output", POSITION, 27),
        start(unavailable, "owned-output", POSITION, 27),
    ] {
        let error = api.control(&control, Some("phone")).err().unwrap();
        assert_eq!(error.status, 400);
    }
    assert!(server.records().is_empty());
}

#[test]
fn a_rejected_spoken_start_is_correlated_and_never_retried() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.mode.store(292, Ordering::SeqCst);
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    provider
        .submit(RemoteAction::Control {
            control: start(chapter, "owned-output", POSITION, 27),
        })
        .unwrap();
    let outcome = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == 27 && result.error.is_some())),
    );
    let RemoteUpdate::State(state) = outcome else {
        unreachable!();
    };
    assert_eq!(state.connection, ProviderConnectionState::Connected);
    assert!(state.issue.is_none());
    let mutating: Vec<_> = server
        .records()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(mutating.len(), 1);
    assert_eq!(
        mutating[0].target,
        "/v1/me/player/play?device_id=owned-output"
    );
}

#[test]
fn spoken_start_supersession_and_confirmation_keep_the_explicit_target_and_position() {
    let chapter = mapping::track(&spoken_fixture("chapter")).unwrap();
    let control = start(chapter.clone(), "fixture-device", POSITION, 27);
    let pause = |device: &str| RemoteControl::PauseOnDevice {
        device_id: device.into(),
    };
    assert!(supersedes(&pause("fixture-device"), &control));
    assert!(!supersedes(&pause("phone"), &control));
    assert!(supersedes(&RemoteControl::Pause, &control));
    assert!(supersedes(
        &control,
        &RemoteControl::Seek { position_ms: 1 }
    ));
    let confirmation = ControlConfirmation {
        control: control.clone(),
        previous: None,
        until: Instant::now(),
        attempts: 0,
    };
    assert!(confirmation.selects_output_or_context());
    let now = Instant::now();
    let mut state = output_base(1);
    let observed = state.playback.as_mut().unwrap();
    observed.current = mapping::track(&spoken_fixture("episode"));
    observed.state = PlaybackState::Playing;
    observed.position_ms = 0;
    assert!(!confirmation.matches(Some(observed)));
    observed.position_ms = POSITION;
    observed.device.as_mut().unwrap().id = "phone".into();
    assert!(!confirmation.matches(Some(observed)));
    observed.device.as_mut().unwrap().id = "fixture-device".into();
    assert!(confirmation.matches(Some(observed)));

    let mut output = output_clock::OutputProjection::default();
    output.remember_selection(&control, now);
    output.ingest(state, now);
    let item = output.take(now).unwrap().playback.unwrap().current.unwrap();
    assert_eq!(item.id, chapter.id);
    assert_eq!(item.artists, chapter.artists);
}
