use super::*;

fn clock(epoch: u64, sequence: u64) -> RemoteOutputClock {
    let mut value = output_clock(sequence);
    value.queue_epoch = Some(epoch);
    value
}

fn entry(id: char, title: &str, source: QueueEntrySource) -> RemoteOutputQueueEntry {
    RemoteOutputQueueEntry {
        media_id: MediaId::parse(format!("spotify:track:{}", id.to_string().repeat(22))).unwrap(),
        source: Some(source),
        metadata: Some(RemoteOutputMetadata {
            title: title.into(),
            artists: vec!["Original artist".into()],
            album: None,
            duration_ms: 0,
            artwork: None,
            context_uri: None,
        }),
    }
}

fn queue(epoch: u64, sequence: u64) -> RemoteOutputQueue {
    let clock = clock(epoch, 1);
    RemoteOutputQueue {
        generation: clock.generation,
        device_id: clock.device_id,
        media_id: clock.media_id.unwrap(),
        epoch,
        sequence,
        autoplay_ready: false,
        previous: vec![
            entry('p', "Previous fixture", QueueEntrySource::Autoplay),
            entry('p', "Previous fixture", QueueEntrySource::Queue),
        ],
        upcoming: vec![
            entry('q', "Queued fixture", QueueEntrySource::Queue),
            entry('q', "Queued fixture", QueueEntrySource::Autoplay),
        ],
    }
}

#[test]
fn owned_queue_survives_web_quota_preserving_order_duplicates_and_explicit_empty() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock(7, 1)),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_observed),
    );
    provider
        .submit(RemoteAction::OutputQueue { queue: queue(7, 1) })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.device_queue.is_some())),
    );

    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = Some("30".into());
    provider.submit(RemoteAction::Refresh).unwrap();
    let RemoteUpdate::State(offline) = wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Unavailable),
    ) else {
        unreachable!()
    };
    let frame = offline.playback.unwrap();
    assert_eq!(frame.upcoming.len(), 2);
    assert_eq!(frame.upcoming[0].title, "Queued fixture");
    assert_eq!(frame.upcoming[0].id, frame.upcoming[1].id);
    let device = frame.device_queue.unwrap();
    assert_eq!(device.previous[0].title, "Previous fixture");
    assert_eq!(device.previous[0].id, device.previous[1].id);
    assert_eq!(
        device.previous_sources,
        [
            Some(QueueEntrySource::Autoplay),
            Some(QueueEntrySource::Queue)
        ]
    );
    assert_eq!(
        device.upcoming_sources,
        [
            Some(QueueEntrySource::Queue),
            Some(QueueEntrySource::Autoplay)
        ]
    );

    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock(8, 2)),
        })
        .unwrap();
    provider
        .submit(RemoteAction::OutputQueue { queue: queue(7, 2) })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.device_queue.is_none() && p.upcoming.is_empty())),
    );
    let mut empty = queue(8, 3);
    empty.previous.clear();
    empty.upcoming.clear();
    provider
        .submit(RemoteAction::OutputQueue { queue: empty })
        .unwrap();
    wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p|
        p.upcoming.is_empty() && p.device_queue.as_ref().is_some_and(|q| q.previous.is_empty() && q.previous_sources.is_empty())))
    });
}

#[test]
fn queue_observation_does_not_advance_playback_and_expires_without_accepting_a_replay() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.ingest(output_base(1), now);
    output.observe(Some(clock(7, 1)), now).unwrap();
    let first = output.take(now).unwrap();
    output.observe_queue(queue(7, 1), now).unwrap();
    let next = output.take(now).unwrap();
    assert_eq!(first.observation_id, next.observation_id);
    assert_eq!(
        first.playback.unwrap().position_ms,
        next.playback.unwrap().position_ms
    );
    for (seconds, sequence) in [(5, 2), (10, 3), (15, 4), (19, 5)] {
        let at = now + Duration::from_secs(seconds);
        output.observe(Some(clock(7, sequence)), at).unwrap();
        let state = output.take(at).unwrap();
        assert_eq!(state.playback.unwrap().device_queue.is_some(), seconds < 18);
    }
    output
        .observe_queue(queue(7, 1), now + Duration::from_secs(19))
        .unwrap();
    assert!(output.take(now + Duration::from_secs(19)).is_none());
}
