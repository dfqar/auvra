use super::*;

#[test]
fn exact_standalone_seed_prefetches_related_music_and_dispatches_once_at_verified_tail() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);

    let initial = output_clock(1);
    let item = mapping::track(&track('1')).unwrap();
    let mut clock = RemoteOutputClock {
        position_ms: 100,
        rate: 1.0,
        control_uptime_ms: Some(1_000),
        queue_epoch: Some(1),
        confirmed_same_media_next: None,
        metadata: Some(RemoteOutputMetadata {
            title: item.title.clone(),
            artists: item.artists.clone(),
            album: item.album.clone(),
            duration_ms: item.duration_ms,
            artwork: None,
            context_uri: None,
        }),
        options: Some(RemoteOutputOptions {
            shuffle: false,
            repeat: RepeatMode::Off,
            enhancement: None,
        }),
        actions: Some(RemoteOutputActions {
            queue_add: true,
            pause: true,
            ..RemoteOutputActions::default()
        }),
        ..initial
    };
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartOnDevice {
                item: item.clone(),
                device_id: clock.device_id.clone(),
                operation_id: Some(4),
            },
        })
        .unwrap();
    let RemoteUpdate::OutputCommand(start) = wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::OutputCommand(command) if matches!(command.control, RemoteOutputControl::StartUri { .. })),
    ) else {
        unreachable!()
    };
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: start.id,
                generation: start.generation,
                accepted: true,
            },
        })
        .unwrap();

    clock.sequence = 2;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    let mut queue = RemoteOutputQueue {
        generation: clock.generation,
        device_id: clock.device_id.clone(),
        media_id: item.id,
        epoch: 1,
        sequence: 1,
        autoplay_ready: false,
        previous: vec![],
        upcoming: vec![],
    };
    provider
        .submit(RemoteAction::OutputQueue {
            queue: queue.clone(),
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        while let Some(update) = provider.poll() {
            assert!(
                !matches!(update, RemoteUpdate::OutputCommand(_)),
                "nothing added in the middle of the song"
            );
        }
        if server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.target.starts_with("/v1/search?"))
        {
            break;
        }
        assert!(Instant::now() < until, "related track lookup did not start");
        thread::sleep(Duration::from_millis(5));
    }

    clock.sequence = 3;
    clock.position_ms = item.duration_ms - 800;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    queue.sequence = 2;
    queue.autoplay_ready = true;
    provider
        .submit(RemoteAction::OutputQueue { queue })
        .unwrap();
    let RemoteUpdate::OutputCommand(candidate) = wait(&mut provider, |update| {
        matches!(
            update, RemoteUpdate::OutputCommand(command) if matches!(command.control, RemoteOutputControl::AutoplayEnqueue { .. })
        )
    }) else {
        unreachable!()
    };
    assert_eq!(
        candidate.control,
        RemoteOutputControl::AutoplayEnqueue {
            uri: format!("spotify:track:{}", "b".repeat(22)),
        }
    );
    let seed_path = format!("/v1/tracks/{}", "1".repeat(22));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.target.split('?').next() == Some(seed_path.as_str()))
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: candidate.id,
                generation: candidate.generation,
                accepted: false,
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::OutputCommand(command) if command.control == RemoteOutputControl::Pause),
    );
    while let Some(update) = provider.poll() {
        assert!(!matches!(
            update,
            RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. }
        ));
    }
}
