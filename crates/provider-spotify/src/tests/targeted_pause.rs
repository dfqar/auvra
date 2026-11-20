use super::*;

struct ReleaseHold(Arc<AtomicBool>);
impl Drop for ReleaseHold {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn pause_clock(sequence: u64) -> RemoteOutputClock {
    let mut clock = output_clock(sequence);
    clock.control_uptime_ms = Some(5_000 + sequence * 250);
    clock.queue_epoch = Some(7);
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        seek: true,
        next: true,
        ..Default::default()
    });
    clock
}

fn bound(server: &Server) -> (SpotifyRemoteProvider, RemoteOutputClock) {
    let mut provider = server.provider();
    connect(&mut provider);
    let clock = pause_clock(1);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    wait(&mut provider, |event| {
        matches!(event,
        RemoteUpdate::State(state) if state.owned_output_control)
    });
    assert!(provider.output.control_clock(Instant::now()).is_some());
    (provider, clock)
}

fn command(provider: &mut SpotifyRemoteProvider) -> RemoteOutputCommand {
    let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
        matches!(event, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    command
}

fn http_pause(provider: &mut SpotifyRemoteProvider, server: &Server, device: &str) {
    let expected = format!("/v1/me/player/pause?device_id={device}");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(event) = provider.poll() {
            assert!(
                !matches!(event, RemoteUpdate::OutputCommand(_)),
                "unexpected native pause: {event:?}"
            );
        }
        if server
            .records()
            .iter()
            .any(|record| record.method == "PUT" && record.target == expected)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "targeted HTTP Pause was not sent"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        server
            .records()
            .iter()
            .filter(
                |record| record.method == "PUT" && record.target.starts_with("/v1/me/player/pause")
            )
            .count(),
        1
    );
}

#[test]
fn targeted_pause_uses_the_exact_bound_native_output_and_request() {
    let server = Server::new();
    let (mut provider, clock) = bound(&server);
    provider
        .submit_pause_on_device_with_request_id(&clock.device_id, 41)
        .unwrap();
    assert!(provider.owned_play_pause_ready_for_request(41));
    assert!(!provider.owned_play_pause_ready_for_request(42));
    assert!(provider.owned_commands.next_requires_control_clock());
    let pause = command(&mut provider);
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.request_id, Some(41));
    assert_eq!(pause.generation, clock.generation);
    assert_eq!(pause.device_id, clock.device_id);
    assert_eq!(Some(pause.media_id), clock.media_id);
    assert_eq!(pause.sequence, clock.sequence);
    assert_eq!(pause.required_queue_epoch, clock.queue_epoch);
    assert!(server.records().iter().all(|record| record.method == "GET"));
}

#[test]
fn targeted_pause_keeps_explicit_http_for_foreign_or_unverified_clocks() {
    for scope in [
        "no clock",
        "foreign target",
        "missing actions",
        "pause disallowed",
        "missing epoch",
        "zero rate",
        "stale",
        "unverified media",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        // Control the clock's age without a wall-clock sleep or a background
        // request introducing a new native clock.
        provider.output = output_clock::OutputProjection::default();
        let now = Instant::now();
        let observed = if scope == "stale" {
            now - output_clock::CONTROL_FRESHNESS - Duration::from_millis(1)
        } else {
            now
        };
        provider.output.ingest(output_base(1), observed);
        if scope != "no clock" {
            let mut clock = pause_clock(1);
            match scope {
                "missing actions" => clock.actions = None,
                "pause disallowed" => clock.actions.as_mut().unwrap().pause = false,
                "missing epoch" => clock.queue_epoch = None,
                "zero rate" => clock.rate = 0.0,
                "unverified media" => {
                    clock.media_id = Some(mapping::track(&track('2')).unwrap().id)
                }
                _ => {}
            }
            provider.output.observe(Some(clock), observed).unwrap();
        }
        let _ = provider.output.take(observed);
        let device = if scope == "foreign target" {
            "external-target"
        } else {
            "fixture-device"
        };
        provider
            .submit_pause_on_device_with_request_id(device, 41)
            .unwrap();
        assert!(!provider.owned_play_pause_ready_for_request(41), "{scope}");
        http_pause(&mut provider, &server, device);
    }
}

#[test]
fn a_positioned_row_witness_never_grants_a_targeted_native_pause() {
    let server = Server::new();
    let (mut provider, mut clock) = bound(&server);
    let now = Instant::now();
    let item = mapping::track(&track('2')).unwrap();
    let context = format!("spotify:album:{}", "a".repeat(22));
    let _ = provider.output.remember_http_selection(
        &RemoteControl::StartContext {
            uri: context.clone(),
            device_id: clock.device_id.clone(),
            item: Some(item.clone()),
            operation_id: Some(27),
            position: Some(0),
            query_id: Some(70),
        },
        now,
    );
    let mut base = output_base(2);
    base.devices
        .push(base.playback.as_ref().unwrap().device.clone().unwrap());
    base.control_result = Some(RemoteControlResult {
        operation_id: 27,
        error: None,
    });
    provider.output.ingest(base, now);
    clock.sequence += 1;
    clock.media_id = Some(item.id.clone());
    clock.position_ms = 1;
    clock.metadata = Some(RemoteOutputMetadata {
        title: item.title,
        artists: item.artists,
        album: None,
        duration_ms: item.duration_ms,
        artwork: None,
        context_uri: Some(context),
    });
    provider.output.observe(Some(clock.clone()), now).unwrap();
    let witnessed = provider.output.take(now).unwrap();
    assert!(witnessed.owned_output_observed && !witnessed.owned_output_control);
    assert!(provider.output.http_navigation_clock(now).is_some());
    assert!(provider.output.control_clock(now).is_none());
    provider
        .submit_pause_on_device_with_request_id(&clock.device_id, 41)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(41));
    http_pause(&mut provider, &server, &clock.device_id);
}

#[test]
fn targeted_pause_waits_behind_native_control_without_losing_its_request_or_clock_policy() {
    let server = Server::new();
    let (mut provider, mut clock) = bound(&server);
    provider.submit_seek_with_request_id(2_000, 40).unwrap();
    let seek = command(&mut provider);
    assert!(matches!(
        seek.control,
        RemoteOutputControl::Seek { position_ms: 2_000 }
    ));
    provider
        .submit_pause_on_device_with_request_id(&clock.device_id, 41)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(41));
    assert!(provider.owned_commands.next_requires_control_clock());
    assert!(!matches!(
        provider.poll(),
        Some(RemoteUpdate::OutputCommand(_))
    ));
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: seek.id,
                generation: seek.generation,
                accepted: true,
            },
        })
        .unwrap();
    assert!(provider.owned_play_pause_ready_for_request(41));
    clock.sequence += 1;
    clock.position_ms = 2_000;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    let pause = command(&mut provider);
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.request_id, Some(41));
    assert_eq!(pause.device_id, clock.device_id);
    assert_eq!(Some(pause.media_id), clock.media_id);
    assert!(server.records().iter().all(|record| record.method == "GET"));
}

#[test]
fn targeted_pause_rechecks_occurrence_and_permission_after_an_inflight_control() {
    for scope in [
        "epoch",
        "media",
        "context",
        "runtime",
        "device",
        "missing actions",
        "pause disallowed",
    ] {
        let server = Server::new();
        let (mut provider, mut clock) = bound(&server);
        provider.submit_seek_with_request_id(2_000, 40).unwrap();
        let seek = command(&mut provider);
        provider
            .submit_pause_on_device_with_request_id(&clock.device_id, 41)
            .unwrap();
        provider
            .submit(RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: seek.id,
                    generation: seek.generation,
                    accepted: true,
                },
            })
            .unwrap();
        clock.sequence += 1;
        match scope {
            "epoch" => clock.queue_epoch = Some(8),
            "media" | "context" => {
                if scope == "media" {
                    clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
                }
                clock.metadata = Some(RemoteOutputMetadata {
                    title: "Original replacement".into(),
                    artists: vec!["Original fixture artist".into()],
                    album: None,
                    duration_ms: 8_000,
                    artwork: None,
                    context_uri: (scope == "context")
                        .then(|| format!("spotify:album:{}", "z".repeat(22))),
                });
            }
            "runtime" => clock.generation += 1,
            "device" => clock.device_id = "other-owned-output".into(),
            "missing actions" => clock.actions = None,
            "pause disallowed" => clock.actions.as_mut().unwrap().pause = false,
            _ => unreachable!(),
        }
        let now = Instant::now();
        provider.output.observe(Some(clock.clone()), now).unwrap();
        let _ = provider.output.take(now);
        assert!(!provider.owned_handoff_ready(), "{scope}");
        let strict = provider.output.control_clock(now);
        let update = provider.owned_commands.poll(strict, now);
        assert!(
            !matches!(update, Some(RemoteUpdate::OutputCommand(_))),
            "{scope}: {update:?}"
        );
        if ["missing actions", "pause disallowed"].contains(&scope) {
            assert!(update.is_none());
            assert!(provider.owned_commands.next_requires_control_clock());
            // A restored full proof within the original deadline may dispatch;
            // loss past that deadline must terminate without an HTTP retry.
            let expired = provider
                .owned_commands
                .poll(strict, now + Duration::from_secs(3));
            assert!(matches!(
                expired,
                Some(RemoteUpdate::OutputCommandFailed {
                    control: RemoteOutputControl::Pause,
                    ..
                })
            ));
        } else if provider.owned_commands.next_requires_control_clock() {
            let _ = provider
                .owned_commands
                .poll(strict, now + Duration::from_secs(3));
        }
        assert!(!provider.owned_play_pause_ready_for_request(41), "{scope}");
        assert!(provider.shared.0.lock().unwrap().controls.is_empty());
        assert!(server.records().iter().all(|record| record.method == "GET"));
    }
}

#[test]
fn targeted_pause_waits_for_full_permission_within_its_original_deadline() {
    let server = Server::new();
    let (mut provider, mut clock) = bound(&server);
    provider.submit_seek_with_request_id(2_000, 40).unwrap();
    let seek = command(&mut provider);
    provider
        .submit_pause_on_device_with_request_id(&clock.device_id, 41)
        .unwrap();
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: seek.id,
                generation: seek.generation,
                accepted: true,
            },
        })
        .unwrap();
    clock.sequence += 1;
    let full_actions = clock.actions.take();
    let now = Instant::now();
    provider.output.observe(Some(clock.clone()), now).unwrap();
    let _ = provider.output.take(now);
    assert!(
        provider
            .owned_commands
            .poll(provider.output.control_clock(now), now)
            .is_none()
    );
    assert!(provider.owned_commands.next_requires_control_clock());
    clock.sequence += 1;
    clock.actions = full_actions;
    let later = now + Duration::from_millis(20);
    provider.output.observe(Some(clock.clone()), later).unwrap();
    let _ = provider.output.take(later);
    let Some(RemoteUpdate::OutputCommand(pause)) = provider
        .owned_commands
        .poll(provider.output.control_clock(later), later)
    else {
        panic!("the same occurrence's full Pause permission should release its waiting intent");
    };
    assert_eq!(pause.request_id, Some(41));
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.required_queue_epoch, clock.queue_epoch);
    assert_eq!(Some(pause.media_id), clock.media_id);
    assert!(server.records().iter().all(|record| record.method == "GET"));
}

#[test]
fn targeted_pause_never_overtakes_an_http_mutation() {
    let server = Server::new();
    let (mut provider, mut clock) = bound(&server);
    server.hold_volume_response.store(true, Ordering::SeqCst);
    let _release = ReleaseHold(server.hold_volume_response.clone());
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Volume { volume: 0.25 },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !server
        .records()
        .iter()
        .any(|record| record.method == "PUT" && record.target.starts_with("/v1/me/player/volume"))
    {
        assert!(
            Instant::now() < deadline,
            "HTTP volume write was not submitted"
        );
        thread::sleep(Duration::from_millis(5));
    }
    clock.sequence += 1;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    provider
        .submit_pause_on_device_with_request_id(&clock.device_id, 41)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(41));
    assert!(!provider.owned_commands.next_requires_control_clock());
    assert!(!matches!(
        provider.poll(),
        Some(RemoteUpdate::OutputCommand(_))
    ));
    server.hold_volume_response.store(false, Ordering::SeqCst);
    http_pause(&mut provider, &server, &clock.device_id);
    let mutations: Vec<_> = server
        .records()
        .into_iter()
        .filter(|record| record.method == "PUT")
        .collect();
    assert_eq!(mutations.len(), 2);
    assert!(mutations[0].target.starts_with("/v1/me/player/volume"));
    assert!(mutations[1].target.starts_with("/v1/me/player/pause"));
}

#[test]
fn uncertain_native_targeted_pause_does_not_fall_back_to_an_http_duplicate() {
    for lost_ack in [false, true] {
        let server = Server::new();
        let (mut provider, clock) = bound(&server);
        provider
            .submit_pause_on_device_with_request_id(&clock.device_id, 41)
            .unwrap();
        let pause = command(&mut provider);
        if lost_ack {
            let failure = provider
                .owned_commands
                .poll(Some(&clock), Instant::now() + Duration::from_secs(4));
            assert!(matches!(
                failure,
                Some(RemoteUpdate::OutputCommandFailed {
                    control: RemoteOutputControl::Pause,
                    ..
                })
            ));
        } else {
            provider
                .submit(RemoteAction::OutputCommandResult {
                    result: RemoteOutputCommandResult {
                        id: pause.id,
                        generation: pause.generation,
                        accepted: false,
                    },
                })
                .unwrap();
            wait(&mut provider, |event| {
                matches!(
                    event,
                    RemoteUpdate::OutputCommandFailed {
                        control: RemoteOutputControl::Pause,
                        ..
                    }
                )
            });
        }
        assert!(provider.shared.0.lock().unwrap().controls.is_empty());
        assert!(!provider.owned_play_pause_ready_for_request(41));
        assert!(server.records().iter().all(|record| record.method == "GET"));
    }
}
