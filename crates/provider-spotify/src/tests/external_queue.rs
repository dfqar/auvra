use super::*;

const QUEUE_PATH: &str = "/v1/me/player/queue";

fn reply(payload: Value) -> QueueReply {
    QueueReply {
        payload,
        hold: None,
    }
}

fn connect_external(provider: &mut SpotifyRemoteProvider) {
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
}

fn observation(provider: &mut SpotifyRemoteProvider, after: u64) -> RemoteState {
    let RemoteUpdate::State(state) = wait(provider, |update| {
        matches!(update,
            RemoteUpdate::State(state) if state.observation_id > after && state.playback.is_some()
        )
    }) else {
        unreachable!()
    };
    *state
}

#[test]
fn external_queue_distinguishes_null_missing_and_malformed_anchors() {
    let episode = spoken_fixture("episode");
    let mut conflicting = track('1');
    conflicting["uri"] = json!(format!("spotify:episode:{}", "1".repeat(22)));
    let responses = Arc::new(Mutex::new(VecDeque::from([
        reply(json!({"currently_playing":null,"queue":[]})),
        reply(json!({"queue":[track('2')]})),
        reply(json!({"currently_playing":{},"queue":[track('2')]})),
        reply(json!({"currently_playing":conflicting,"queue":[track('2')]})),
        reply(json!({"currently_playing":episode,"queue":vec![track('2');24]})),
    ])));
    let server =
        SlowFixture::with_playback_and_queue("", 200, Arc::new(Mutex::new(frame())), responses);
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut api = Api::new(server.base.clone(), shared).unwrap();
    api.token = Some("fixture-bearer".into());
    let empty = api
        .queue()
        .unwrap_or_else(|_| panic!("explicit empty queue"));
    assert!(empty.current.is_none() && empty.upcoming.is_empty());
    for _ in 0..3 {
        assert!(api.queue().is_err());
    }
    let queue = api.queue().unwrap_or_else(|_| panic!("episode anchor"));
    assert_eq!(queue.current, mapping::track(&episode).map(|item| item.id));
    assert_eq!(queue.upcoming.len(), 19);
}

#[test]
fn external_queue_ignores_foreign_track_and_replaces_matching_queue_with_empty() {
    let hold_matching = Arc::new(AtomicBool::new(true));
    let hold_empty = Arc::new(AtomicBool::new(true));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        reply(json!({"currently_playing":track('3'),"queue":[track('x')]})),
        QueueReply {
            payload: json!({"currently_playing":track('1'),"queue":[track('2')]}),
            hold: Some(hold_matching.clone()),
        },
        QueueReply {
            payload: json!({"currently_playing":track('1'),"queue":[]}),
            hold: Some(hold_empty.clone()),
        },
    ])));
    let server =
        SlowFixture::with_playback_and_queue("", 200, Arc::new(Mutex::new(frame())), replies);
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    connect_external(&mut provider);
    let initial = observation(&mut provider, 0);
    server.wait_for(QUEUE_PATH);
    provider.submit(RemoteAction::Refresh).unwrap();
    let refreshed = observation(&mut provider, initial.observation_id);
    server.wait_for_count(QUEUE_PATH, 2);
    // The second HTTP read cannot finish yet. A queue projected now would have
    // come from the first reply, whose anchor names a different recording.
    provider.submit(RemoteAction::Refresh).unwrap();
    let current = observation(&mut provider, refreshed.observation_id);
    assert!(current.playback.unwrap().upcoming.is_empty());
    hold_matching.store(false, Ordering::SeqCst);
    let expected = mapping::track(&track('2')).unwrap().id;
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.playback.as_ref().is_some_and(|frame| frame.upcoming.first().is_some_and(|item| item.id == expected)))
    });

    server.wait_for_count(QUEUE_PATH, 3);
    hold_empty.store(false, Ordering::SeqCst);
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.playback.as_ref().is_some_and(|frame| frame.upcoming.is_empty()))
    });
}

#[test]
fn returning_to_the_original_external_device_and_item_cannot_resurrect_a_racing_queue() {
    let held_old = Arc::new(AtomicBool::new(true));
    let held_current = Arc::new(AtomicBool::new(true));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        QueueReply {
            payload: json!({"currently_playing":track('1'),"queue":[track('x')]}),
            hold: Some(held_old.clone()),
        },
        QueueReply {
            payload: json!({"currently_playing":track('1'),"queue":[track('2')]}),
            hold: Some(held_current.clone()),
        },
    ])));
    let playback = Arc::new(Mutex::new(frame()));
    let server = SlowFixture::with_playback_and_queue("", 200, playback.clone(), replies);
    let mut provider = SpotifyRemoteProvider::with_base(server.base.clone()).unwrap();
    connect_external(&mut provider);
    let initial = observation(&mut provider, 0);
    server.wait_for(QUEUE_PATH);

    {
        let mut playback = playback.lock().unwrap();
        playback["device"]["id"] = json!("phone");
        playback["item"] = track('3');
    }
    provider.submit(RemoteAction::Refresh).unwrap();
    let phone = observation(&mut provider, initial.observation_id);
    assert_eq!(phone.playback.unwrap().device.unwrap().id, "phone");
    *playback.lock().unwrap() = frame();
    provider.submit(RemoteAction::Refresh).unwrap();
    let returned = observation(&mut provider, phone.observation_id);
    assert_eq!(
        returned.playback.unwrap().device.unwrap().id,
        "fixture-device"
    );

    held_old.store(false, Ordering::SeqCst);
    server.wait_for_count(QUEUE_PATH, 2);
    provider.submit(RemoteAction::Refresh).unwrap();
    let after_old = observation(&mut provider, returned.observation_id);
    assert!(after_old.playback.unwrap().upcoming.is_empty());
    held_current.store(false, Ordering::SeqCst);
    let expected = mapping::track(&track('2')).unwrap().id;
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::State(state)
        if state.playback.as_ref().is_some_and(|frame| frame.upcoming.first().is_some_and(|item| item.id == expected)))
    });
}
