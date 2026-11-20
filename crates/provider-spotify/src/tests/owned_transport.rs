use super::*;

fn controllable_clock(sequence: u64) -> RemoteOutputClock {
    let mut clock = output_clock(sequence);
    clock.control_uptime_ms = Some(5000 + sequence * 250);
    clock.volume = Some(0.4);
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        next: true,
        seek: true,
        ..Default::default()
    });
    clock
}

#[test]
fn owned_next_ready_checks_the_exact_front_request_and_available_dispatch_credit() {
    let now = Instant::now();
    let clock = controllable_clock(1);
    let mut commands = owned_commands::OwnedCommands::default();
    commands
        .push(RemoteOutputControl::Pause, &clock, now)
        .unwrap();
    commands
        .push_with_request_id(RemoteOutputControl::Next, &clock, now, Some(41))
        .unwrap();
    assert!(!commands.owned_next_ready_for_request(41));
    let Some(RemoteUpdate::OutputCommand(pause)) = commands.poll(Some(&clock), now) else {
        unreachable!()
    };
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert!(!commands.owned_next_ready_for_request(41));
    commands.acknowledge(RemoteOutputCommandResult {
        id: pause.id,
        generation: pause.generation,
        accepted: true,
    });
    assert!(commands.owned_next_ready_for_request(41));
    assert!(!commands.owned_next_ready_for_request(42));
    commands.revoke();
    commands
        .push(RemoteOutputControl::Next, &clock, now)
        .unwrap();
    assert!(!commands.owned_next_ready_for_request(41));
}

#[test]
fn standard_owned_previous_preserves_the_request_without_requiring_a_cue() {
    let now = Instant::now();
    let mut clock = controllable_clock(1);
    clock.actions.as_mut().unwrap().previous = true;
    clock.queue_epoch = Some(41);
    let mut commands = owned_commands::OwnedCommands::default();
    commands
        .push_with_request_id(RemoteOutputControl::Previous, &clock, now, Some(42))
        .unwrap();
    let mut dispatch_clock = clock.clone();
    dispatch_clock.sequence += 1;
    dispatch_clock.queue_epoch = Some(42);
    let Some(RemoteUpdate::OutputCommand(previous)) = commands.poll(Some(&dispatch_clock), now)
    else {
        panic!("Expected a native Previous command");
    };
    assert_eq!(previous.control, RemoteOutputControl::Previous);
    assert_eq!(previous.request_id, Some(42));
    assert_eq!(previous.required_queue_epoch, None);
    assert_eq!(previous.observed_queue_epoch, dispatch_clock.queue_epoch);
    assert_eq!(previous.sequence, dispatch_clock.sequence);
    assert_eq!(previous.expected_previous_media_id, None);
}

#[test]
fn correlated_seek_preserves_owned_fifo_and_has_no_http_dispatch_credit() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider.submit_seek_with_request_id(2_000, 40).unwrap();
    assert!(!provider.owned_seek_ready_for_request(40));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !server
        .records()
        .iter()
        .any(|request| request.method == "PUT" && request.target.starts_with("/v1/me/player/seek?"))
    {
        assert!(Instant::now() < deadline, "HTTP seek was not submitted");
        thread::sleep(Duration::from_millis(5));
    }

    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let clock = controllable_clock(1);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    wait(
        &mut provider,
        |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    provider.submit_seek_with_request_id(1_250, 41).unwrap();
    assert!(!provider.owned_seek_ready_for_request(41));
    let take_command = |provider: &mut SpotifyRemoteProvider| {
        let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
            matches!(event, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        command
    };
    let pause = take_command(&mut provider);
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert!(!provider.owned_seek_ready_for_request(41));
    provider.submit_seek_with_request_id(2_500, 42).unwrap();
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: pause.id,
                generation: pause.generation,
                accepted: true,
            },
        })
        .unwrap();
    assert!(!provider.owned_seek_ready_for_request(41));
    assert!(provider.owned_seek_ready_for_request(42));
    assert!(!provider.owned_next_ready_for_request(42));
    let seek = take_command(&mut provider);
    assert_eq!(
        seek.control,
        RemoteOutputControl::Seek { position_ms: 2_500 }
    );
    assert_eq!(seek.request_id, Some(42));
    assert_eq!(seek.device_id, clock.device_id);
    assert_eq!(seek.generation, clock.generation);
    assert_eq!(Some(seek.media_id), clock.media_id);
    assert!(!provider.owned_seek_ready_for_request(42));
    assert!(
        server
            .records()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[test]
fn correlated_play_pause_keeps_native_fifo_and_external_http_separate() {
    let external = Server::new();
    let mut provider = external.provider();
    connect(&mut provider);
    provider
        .submit_play_pause_with_request_id(false, 40)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(40));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !external
        .records()
        .iter()
        .any(|request| request.method == "PUT" && request.target.starts_with("/v1/me/player/pause"))
    {
        assert!(Instant::now() < deadline, "HTTP pause was not submitted");
        thread::sleep(Duration::from_millis(5));
    }

    let native = Server::new();
    let mut provider = native.provider();
    connect(&mut provider);
    let mut paused = controllable_clock(1);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    paused.actions.as_mut().unwrap().play = true;
    paused.actions.as_mut().unwrap().pause = false;
    paused.queue_epoch = Some(7);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(paused.clone()),
        })
        .unwrap();
    wait(
        &mut provider,
        |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
    );
    let take_command = |provider: &mut SpotifyRemoteProvider| {
        let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
            matches!(event, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        command
    };

    provider
        .submit_play_pause_with_request_id(true, 41)
        .unwrap();
    assert!(provider.owned_play_pause_ready_for_request(41));
    assert!(!provider.owned_play_pause_ready_for_request(40));
    assert!(!provider.owned_seek_ready_for_request(41));
    let play = take_command(&mut provider);
    assert_eq!(play.control, RemoteOutputControl::Play);
    assert_eq!(play.request_id, Some(41));
    provider
        .submit_play_pause_with_request_id(false, 42)
        .unwrap();
    assert!(!provider.owned_play_pause_ready_for_request(42));
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: play.id,
                generation: play.generation,
                accepted: true,
            },
        })
        .unwrap();
    assert!(provider.owned_play_pause_ready_for_request(42));
    let mut playing = controllable_clock(2);
    playing.queue_epoch = paused.queue_epoch;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(playing),
        })
        .unwrap();
    let pause = take_command(&mut provider);
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.request_id, Some(42));
    assert_eq!(pause.device_id, paused.device_id);
    assert_eq!(pause.generation, paused.generation);
    assert_eq!(Some(pause.media_id), paused.media_id);
    assert!(
        native
            .records()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[test]
fn same_media_next_publishes_only_the_correlated_owned_request_in_either_ack_order() {
    for (ack_first, resume_into_loading) in
        [(true, false), (false, false), (true, true), (false, true)]
    {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.queue_epoch = Some(7);
        if resume_into_loading {
            clock.state = PlaybackState::Paused;
            clock.rate = 0.0;
            clock.actions.as_mut().unwrap().play = true;
        }
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
        );

        if resume_into_loading {
            provider
                .submit(RemoteAction::Control {
                    control: RemoteControl::Play,
                })
                .unwrap();
            let RemoteUpdate::OutputCommand(play) = wait(&mut provider, |u| {
                matches!(u, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            assert_eq!(play.control, RemoteOutputControl::Play);
            provider.submit_next_with_request_id(41).unwrap();
            clock.state = PlaybackState::Loading;
            clock.sequence += 1;
            let loading = RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            };
            let accepted = RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: play.id,
                    generation: play.generation,
                    accepted: true,
                },
            };
            let (first, second) = if ack_first {
                (accepted, loading)
            } else {
                (loading, accepted)
            };
            provider.submit(first).unwrap();
            assert!(!provider.owned_handoff_ready());
            provider.submit(second).unwrap();
            assert!(provider.owned_handoff_ready());
        } else {
            provider.submit_next_with_request_id(41).unwrap();
        }
        let RemoteUpdate::OutputCommand(sent) = wait(&mut provider, |u| {
            matches!(u, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert_eq!(sent.request_id, Some(41));
        assert_eq!(sent.required_queue_epoch, resume_into_loading.then_some(7));
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Pause,
            })
            .unwrap();
        let ack = || RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: sent.id,
                generation: sent.generation,
                accepted: true,
            },
        };
        if ack_first {
            provider.submit(ack()).unwrap();
        }

        clock.sequence += 1;
        clock.queue_epoch = Some(8);
        clock.position_ms = 3;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.confirmed_same_media_next = Some(sent.id);
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        if !ack_first {
            let RemoteUpdate::State(s) = wait(
                &mut provider,
                |u| matches!(u, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.position_ms == 3)),
            ) else {
                unreachable!()
            };
            assert_eq!(s.confirmed_same_media_next_request_id, None);
            // A following brief ordinary observation lacks the marker.
            clock.sequence += 1;
            clock.confirmed_same_media_next = None;
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
            provider.submit(ack()).unwrap();
        }
        let RemoteUpdate::State(s) = wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.confirmed_same_media_next_request_id == Some(41)),
        ) else {
            unreachable!()
        };
        assert!(s.owned_output_control);
        assert_eq!(s.playback.as_ref().unwrap().position_ms, 3);
        let RemoteUpdate::OutputCommand(pause) = wait(&mut provider, |u| {
            matches!(u, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert_eq!(pause.control, RemoteOutputControl::Pause);
        assert_eq!(pause.request_id, None);
        assert!(server.records().iter().all(|record| record.method == "GET"));
    }
}

#[test]
fn same_media_next_proof_does_not_follow_epochs_failures_or_a_newer_next() {
    let now = Instant::now();
    let mut clock = controllable_clock(1);
    clock.queue_epoch = Some(7);
    let mut commands = super::super::owned_commands::OwnedCommands::default();
    let submit_next = |commands: &mut super::super::owned_commands::OwnedCommands,
                       clock: &RemoteOutputClock,
                       request_id| {
        commands
            .push_with_request_id(RemoteOutputControl::Next, clock, now, Some(request_id))
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(sent)) = commands.poll(Some(clock), now) else {
            unreachable!()
        };
        sent
    };
    let ack = |sent: &RemoteOutputCommand, accepted| RemoteOutputCommandResult {
        id: sent.id,
        generation: sent.generation,
        accepted,
    };
    let old = submit_next(&mut commands, &clock, 100);
    commands.acknowledge(ack(&old, true));
    clock.sequence += 1;
    clock.queue_epoch = Some(8);
    assert_eq!(commands.project_same_media_next(Some(&clock), now), None);
    let new = submit_next(&mut commands, &clock, 101);
    commands.acknowledge(ack(&new, true));

    clock.sequence += 1;
    clock.queue_epoch = Some(9);
    clock.confirmed_same_media_next = Some(old.id);
    assert_eq!(commands.project_same_media_next(Some(&clock), now), None);
    clock.confirmed_same_media_next = Some(new.id);
    for invalid in [
        "media",
        "device",
        "generation",
        "sequence",
        "epoch",
        "actions",
        "rate",
    ] {
        let mut stale = clock.clone();
        match invalid {
            "media" => {
                stale.media_id =
                    Some(MediaId::parse(format!("spotify:track:{}", "9".repeat(22))).unwrap())
            }
            "device" => stale.device_id = "other-device".into(),
            "generation" => stale.generation += 1,
            "sequence" => stale.sequence = new.sequence,
            "epoch" => stale.queue_epoch = Some(8),
            "actions" => stale.actions = None,
            "rate" => stale.rate = 0.0,
            _ => unreachable!(),
        }
        assert_eq!(
            commands.project_same_media_next(Some(&stale), now),
            None,
            "{invalid}"
        );
    }
    assert_eq!(
        commands.project_same_media_next(Some(&clock), now),
        Some(101)
    );
    clock.confirmed_same_media_next = None;
    assert_eq!(
        commands.project_same_media_next(Some(&clock), now),
        Some(101)
    );
    clock.queue_epoch = Some(10);
    assert_eq!(commands.project_same_media_next(Some(&clock), now), None);
    let rejected = submit_next(&mut commands, &clock, 102);
    clock.sequence += 1;
    clock.queue_epoch = Some(11);
    clock.confirmed_same_media_next = Some(rejected.id);
    assert_eq!(commands.project_same_media_next(Some(&clock), now), None);
    commands.acknowledge(ack(&rejected, false));
    assert_eq!(commands.project_same_media_next(Some(&clock), now), None);
}

#[test]
fn queued_resume_or_skip_from_paused_keeps_local_handoff_pending_before_dispatch() {
    for control in [RemoteControl::Play, RemoteControl::Next] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        let actions = clock.actions.as_mut().unwrap();
        actions.play = true;
        actions.previous = true;
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control && state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Paused)),
        );

        provider
            .submit(RemoteAction::Control {
                control: control.clone(),
            })
            .unwrap();
        assert!(provider.owned_commands.next_is_output_transport());
        assert!(
            provider.has_pending_output_start(),
            "queued {control:?} must defer local playback"
        );
        let RemoteUpdate::OutputCommand(sent) = wait(&mut provider, |update| {
            matches!(update, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert!(provider.has_pending_output_start());
        provider
            .submit(RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: sent.id,
                    generation: sent.generation,
                    accepted: true,
                },
            })
            .unwrap();
        assert!(provider.has_pending_output_start());

        clock.sequence += 1;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        assert!(provider.has_pending_output_start());
        wait(
            &mut provider,
            |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control && state.playback.as_ref().is_some_and(|frame| frame.state == PlaybackState::Playing)),
        );
        assert!(!provider.has_pending_output_start());
        assert!(
            server
                .records()
                .iter()
                .all(|request| request.method == "GET")
        );
    }
}

#[test]
fn queued_skip_while_playing_leaves_the_immediate_local_pause_path_available() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(controllable_clock(1)),
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    assert!(provider.owned_commands.next_is_output_transport());
    assert!(!provider.has_pending_output_start());
    wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    });
    assert!(!provider.has_pending_output_start());
}

#[test]
fn targeted_context_starts_locally_under_quota_and_waits_for_fresh_playback() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(controllable_clock(1)),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = Some("30".into());
    provider.submit(RemoteAction::Refresh).unwrap();
    wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s)
        if s.connection == ProviderConnectionState::Unavailable && s.owned_output_control)
    });

    let uri = format!("spotify:album:{}", "a".repeat(22));
    let start = |device: &str, operation_id| RemoteAction::Control {
        control: RemoteControl::StartContext {
            uri: uri.clone(),
            device_id: device.into(),
            operation_id: Some(operation_id),
            item: None,
            position: None,
            query_id: None,
        },
    };
    assert!(provider.submit(start("other-device", 40)).is_err());
    provider.submit(start("fixture-device", 41)).unwrap();
    let RemoteUpdate::OutputCommand(sent) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(
        sent.control,
        RemoteOutputControl::StartUri {
            uri: uri.clone(),
            operation_id: 41
        }
    );
    assert!(provider.has_pending_output_start());
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: sent.id,
                generation: sent.generation,
                accepted: true,
            },
        })
        .unwrap();
    let RemoteUpdate::State(receipt) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s)
        if s.control_result.as_ref().is_some_and(|r| r.operation_id == 41 && r.error.is_none()))
    }) else {
        unreachable!()
    };
    assert_eq!(receipt.playback.unwrap().context_uri, None);
    assert!(provider.has_pending_output_start());

    let mut changed = controllable_clock(2);
    changed.position_ms = 250;
    changed.metadata = Some(RemoteOutputMetadata {
        title: "Owned fixture".into(),
        artists: vec![],
        album: None,
        duration_ms: 8000,
        artwork: None,
        context_uri: Some(uri.clone()),
    });
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(changed),
        })
        .unwrap();
    wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s)
        if s.playback.as_ref().and_then(|p| p.context_uri.as_ref()) == Some(&uri))
    });
    assert!(!provider.has_pending_output_start());
    assert!(server.records().iter().all(|r| r.method == "GET"));
}

#[test]
fn queued_owned_selection_keeps_latest_intent_and_reports_only_its_rejection() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(controllable_clock(1)),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    for (id, operation) in [('2', 1), ('3', 2)] {
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::StartOnDevice {
                    item: mapping::track(&track(id)).unwrap(),
                    device_id: "fixture-device".into(),
                    operation_id: Some(operation),
                },
            })
            .unwrap();
    }
    let RemoteUpdate::OutputCommand(sent) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(
        sent.control,
        RemoteOutputControl::StartUri {
            uri: format!("spotify:track:{}", "3".repeat(22)),
            operation_id: 2
        }
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::StartOnDevice {
                item: mapping::track(&track('4')).unwrap(),
                device_id: "fixture-device".into(),
                operation_id: Some(3),
            },
        })
        .unwrap();
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: sent.id,
                generation: sent.generation,
                accepted: false,
            },
        })
        .unwrap();
    wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s)
        if s.control_result.as_ref().is_some_and(|r| r.operation_id == 2 && r.error.is_some()))
    });
    let RemoteUpdate::OutputCommand(next) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(next.control.operation_id(), Some(3));
    assert!(server.records().iter().all(|r| r.method == "GET"));
}

#[test]
fn local_pause_is_dispatched_during_http_quota_and_observation_confirms_pause() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(controllable_clock(1)),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = Some("30".into());
    provider.submit(RemoteAction::Refresh).unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.connection == ProviderConnectionState::Unavailable && s.owned_output_control),
    );

    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    let RemoteUpdate::OutputCommand(sent) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(sent.control, RemoteOutputControl::Pause);
    assert_eq!(sent.device_id, "fixture-device");
    assert_eq!(sent.expires_at_uptime_ms, 6750);
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: sent.id,
                generation: sent.generation,
                accepted: true,
            },
        })
        .unwrap();
    while let Some(update) = provider.poll() {
        assert!(
            !matches!(update, RemoteUpdate::State(s) if s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused))
        );
    }
    let mut paused = controllable_clock(2);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    paused.actions = Some(RemoteOutputActions {
        play: true,
        ..Default::default()
    });
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(paused),
        })
        .unwrap();
    wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s) if s.owned_output_control
        && s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Paused && p.capabilities.play && !p.capabilities.pause))
    });
    assert!(server.records().iter().all(|r| r.method == "GET"));
}

#[test]
fn an_owned_clock_without_action_advertisement_keeps_web_api_pause() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = controllable_clock(1);
    clock.actions = None;
    provider
        .submit(RemoteAction::OutputClock { clock: Some(clock) })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !server
        .records()
        .iter()
        .any(|r| r.method == "PUT" && r.target.starts_with("/v1/me/player/pause"))
    {
        assert!(!matches!(
            provider.poll(),
            Some(RemoteUpdate::OutputCommand(_))
        ));
        assert!(
            Instant::now() < until,
            "HTTP fallback did not dispatch Pause"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_stable_owned_clock_without_actions_keeps_web_api_navigation() {
    for (control, target) in [
        (
            RemoteControl::Next,
            "/v1/me/player/next?device_id=fixture-device",
        ),
        (
            RemoteControl::Previous,
            "/v1/me/player/previous?device_id=fixture-device",
        ),
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.actions = None;
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        wait(&mut provider, |u| {
            matches!(u, RemoteUpdate::State(s) if s.owned_output_control
                && s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Playing
                    && p.transport.next && p.transport.previous))
        });

        if matches!(control, RemoteControl::Next) {
            provider.submit_next_with_request_id(41).unwrap();
            assert!(!provider.owned_next_ready_for_request(41));
        } else {
            provider.submit(RemoteAction::Control { control }).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server
            .records()
            .iter()
            .any(|r| r.method == "POST" && r.target == target)
        {
            assert!(!matches!(
                provider.poll(),
                Some(RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. })
            ));
            assert!(Instant::now() < deadline, "Missing HTTP POST to {target}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn relative_skips_survive_owned_track_changes_without_waiting_for_http_metadata() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut original = controllable_clock(1);
    original.actions.as_mut().unwrap().previous = true;
    original.position_ms = 800;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(original),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    let next = || RemoteAction::Control {
        control: RemoteControl::Next,
    };
    let dispatch = |provider: &mut SpotifyRemoteProvider| match wait(provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    }) {
        RemoteUpdate::OutputCommand(command) => command,
        _ => unreachable!(),
    };
    let accept = |command: &RemoteOutputCommand| RemoteAction::OutputCommandResult {
        result: RemoteOutputCommandResult {
            id: command.id,
            generation: command.generation,
            accepted: true,
        },
    };
    provider.submit(next()).unwrap();
    let first = dispatch(&mut provider);
    provider.submit(next()).unwrap();

    let mut changing = controllable_clock(2);
    let item = mapping::track(&track('2')).unwrap();
    let changed = item.id;
    changing.media_id = Some(changed.clone());
    changing.state = PlaybackState::Loading;
    changing.actions = None;
    changing.metadata = Some(RemoteOutputMetadata {
        title: item.title,
        artists: item.artists,
        album: item.album,
        duration_ms: item.duration_ms,
        artwork: item.artwork,
        context_uri: None,
    });
    server.delay.store(800, Ordering::SeqCst);
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(changing.clone()),
        })
        .unwrap();
    let RemoteUpdate::State(state) = wait(&mut provider, |u| {
        matches!(u, RemoteUpdate::State(s) if s.owned_output_control
            && s.playback.as_ref().is_some_and(|p| p.state == PlaybackState::Loading
                && p.current.as_ref().is_some_and(|current| current.id == changed)))
    }) else {
        unreachable!()
    };
    let loading = state.playback.unwrap();
    assert!(loading.transport.next && loading.transport.previous);
    assert!(!loading.capabilities.seek && !loading.capabilities.pause);
    provider.submit(accept(&first)).unwrap();
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Previous,
        })
        .unwrap();
    while let Some(update) = provider.poll() {
        assert!(!matches!(
            update,
            RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. }
        ));
    }

    changing.sequence = 3;
    changing.state = PlaybackState::Playing;
    changing.actions = Some(RemoteOutputActions {
        next: true,
        previous: true,
        ..Default::default()
    });
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(changing),
        })
        .unwrap();
    for expected in [RemoteOutputControl::Next, RemoteOutputControl::Previous] {
        let sent = dispatch(&mut provider);
        assert_eq!(sent.control, expected);
        assert_eq!(sent.media_id, changed);
        assert_eq!(sent.sequence, 3);
        provider.submit(accept(&sent)).unwrap();
    }
    assert!(server.records().iter().all(|record| record.method == "GET"));
    server.delay.store(0, Ordering::SeqCst);
}

#[test]
fn previous_from_the_first_track_waits_for_the_owned_next() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = controllable_clock(1);
    let device = clock.device_id.clone();
    let original = clock.media_id.clone().unwrap();
    let observe = |provider: &mut SpotifyRemoteProvider, clock| {
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
    };
    let dispatch = |provider: &mut SpotifyRemoteProvider| match wait(provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    }) {
        RemoteUpdate::OutputCommand(command) => command,
        _ => unreachable!(),
    };
    let accept = |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand| {
        provider
            .submit(RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: command.id,
                    generation: command.generation,
                    accepted: true,
                },
            })
            .unwrap();
    };
    observe(&mut provider, clock.clone());
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
    );
    assert!(provider.submit_previous_after_next(&device).is_err());

    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    assert!(
        provider
            .submit_previous_after_next("another-device")
            .is_err()
    );
    provider.submit_previous_after_next(&device).unwrap();
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    let next = dispatch(&mut provider);
    assert_eq!(
        (next.control.clone(), &next.media_id),
        (RemoteOutputControl::Next, &original)
    );
    accept(&mut provider, &next);
    clock.sequence += 1;
    clock.position_ms = 15_000;
    observe(&mut provider, clock.clone());
    while let Some(update) = provider.poll() {
        assert!(!matches!(
            update,
            RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. }
        ));
    }

    clock.sequence += 1;
    clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
    clock.position_ms = 0;
    clock.actions.as_mut().unwrap().previous = true;
    observe(&mut provider, clock.clone());
    let previous = dispatch(&mut provider);
    assert_eq!(previous.control, RemoteOutputControl::Previous);
    assert_eq!(Some(&previous.media_id), clock.media_id.as_ref());
    accept(&mut provider, &previous);
    clock.sequence += 1;
    clock.media_id = Some(original.clone());
    clock.actions.as_mut().unwrap().previous = false;
    observe(&mut provider, clock);
    let pause = dispatch(&mut provider);
    assert_eq!(
        (pause.control, pause.media_id),
        (RemoteOutputControl::Pause, original)
    );
    assert!(server.records().iter().all(|record| record.method == "GET"));
}

#[test]
fn play_pause_follow_the_confirmed_owned_navigation_and_preserve_the_latest_toggle() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = controllable_clock(1);
    clock.actions.as_mut().unwrap().play = false;
    let observe = |provider: &mut SpotifyRemoteProvider, clock| {
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        wait(
            provider,
            |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
        );
    };
    let send = |provider: &mut SpotifyRemoteProvider, control| {
        provider.submit(RemoteAction::Control { control }).unwrap();
    };
    let dispatch = |provider: &mut SpotifyRemoteProvider| match wait(provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    }) {
        RemoteUpdate::OutputCommand(command) => command,
        _ => unreachable!(),
    };
    let accept = |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand| {
        provider
            .submit(RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: command.id,
                    generation: command.generation,
                    accepted: true,
                },
            })
            .unwrap();
    };
    let no_dispatch = |provider: &mut SpotifyRemoteProvider| {
        while let Some(update) = provider.poll() {
            assert!(!matches!(
                update,
                RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. }
            ));
        }
    };
    observe(&mut provider, clock.clone());
    send(&mut provider, RemoteControl::Next);
    let next = dispatch(&mut provider);
    send(&mut provider, RemoteControl::Pause);
    accept(&mut provider, &next);
    no_dispatch(&mut provider);

    let new_item = mapping::track(&track('2')).unwrap();
    clock.sequence = 2;
    clock.media_id = Some(new_item.id.clone());
    clock.state = PlaybackState::Loading;
    clock.rate = 0.0;
    clock.actions = None;
    clock.metadata = Some(RemoteOutputMetadata {
        title: new_item.title,
        artists: new_item.artists,
        album: new_item.album,
        duration_ms: new_item.duration_ms,
        artwork: new_item.artwork,
        context_uri: None,
    });
    observe(&mut provider, clock.clone());
    no_dispatch(&mut provider);
    clock.sequence = 3;
    clock.state = PlaybackState::Playing;
    clock.rate = 1.0;
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        ..Default::default()
    });
    observe(&mut provider, clock.clone());
    let pause = dispatch(&mut provider);
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.media_id, new_item.id);
    send(&mut provider, RemoteControl::Play);
    accept(&mut provider, &pause);
    no_dispatch(&mut provider);
    clock.sequence = 4;
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.actions = Some(RemoteOutputActions {
        play: true,
        ..Default::default()
    });
    observe(&mut provider, clock);
    let play = dispatch(&mut provider);
    assert_eq!(play.control, RemoteOutputControl::Play);
    assert_eq!(play.media_id, new_item.id);
    assert!(server.records().iter().all(|record| record.method == "GET"));
}

#[test]
fn loading_navigation_requires_recent_actions_from_the_same_owned_binding() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.ingest(output_base(1), now);
    output.observe(Some(controllable_clock(1)), now).unwrap();
    assert!(output.take(now).unwrap().playback.unwrap().transport.next);

    let mut loading = controllable_clock(2);
    loading.state = PlaybackState::Loading;
    loading.actions = None;
    output.observe(Some(loading.clone()), now).unwrap();
    let projected = output.take(now).unwrap().playback.unwrap().transport;
    assert!(projected.next && !projected.previous);

    let later = now + Duration::from_secs(1);
    loading.sequence = 3;
    output.observe(Some(loading.clone()), later).unwrap();
    let expired = output.take(later).unwrap().playback.unwrap().transport;
    assert!(!expired.next && !expired.previous);

    output.observe(Some(controllable_clock(4)), later).unwrap();
    assert!(output.take(later).unwrap().playback.unwrap().transport.next);
    loading.generation += 1;
    loading.sequence = 1;
    output.observe(Some(loading), later).unwrap();
    let rebound = output.take(later).unwrap().playback.unwrap().transport;
    assert!(!rebound.next && !rebound.previous);
}

#[test]
fn queued_skips_expire_or_are_revoked_without_dispatch() {
    let now = Instant::now();
    let mut commands = owned_commands::OwnedCommands::default();
    let mut clock = controllable_clock(1);
    clock.actions = None;
    commands
        .push(RemoteOutputControl::Next, &clock, now)
        .unwrap();
    assert!(commands.poll(Some(&clock), now).is_none());
    assert!(matches!(
        commands.poll(Some(&clock), now + Duration::from_secs(2)),
        Some(RemoteUpdate::OutputCommandFailed {
            control: RemoteOutputControl::Next,
            ..
        })
    ));
    commands
        .push(RemoteOutputControl::Next, &clock, now)
        .unwrap();
    commands.revoke_output_transport();
    clock = controllable_clock(2);
    assert!(commands.poll(Some(&clock), now).is_none());
}

#[test]
fn only_pause_waits_for_a_slow_owned_navigation_successor() {
    let now = Instant::now();
    let clock = controllable_clock(1);
    for (control, successor_at, different_item, should_dispatch) in [
        (RemoteOutputControl::Pause, 2708, true, true),
        (RemoteOutputControl::Pause, 3500, true, false),
        (RemoteOutputControl::Pause, 2708, false, false),
        (RemoteOutputControl::Play, 2708, true, false),
    ] {
        let mut commands = owned_commands::OwnedCommands::default();
        commands
            .push(RemoteOutputControl::Next, &clock, now)
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(next)) = commands.poll(Some(&clock), now) else {
            panic!("Expected an owned Next")
        };
        commands
            .push(control.clone(), &clock, now + Duration::from_millis(29))
            .unwrap();
        commands.acknowledge(RemoteOutputCommandResult {
            id: next.id,
            generation: next.generation,
            accepted: true,
        });
        let mut loading = clock.clone();
        loading.sequence += 1;
        loading.state = PlaybackState::Loading;
        loading.rate = 0.0;
        loading.actions = None;
        assert!(
            commands
                .poll(Some(&loading), now + Duration::from_millis(300))
                .is_none()
        );
        let mut successor = controllable_clock(3);
        if different_item {
            successor.media_id = Some(mapping::track(&track('2')).unwrap().id);
        }
        let result = commands.poll(Some(&successor), now + Duration::from_millis(successor_at));
        if should_dispatch {
            let Some(RemoteUpdate::OutputCommand(pause)) = result else {
                panic!("Expected Pause on the successor")
            };
            assert_eq!(pause.control, RemoteOutputControl::Pause);
            assert_eq!(Some(pause.media_id), successor.media_id);
        } else {
            assert!(matches!(
                result,
                Some(RemoteUpdate::OutputCommandFailed { .. })
            ));
        }
    }
}

#[test]
fn trailing_pause_uses_the_acknowledged_last_navigation_of_a_burst() {
    let now = Instant::now();
    let track_clock = |sequence, id| {
        let mut clock = controllable_clock(sequence);
        clock.media_id = Some(mapping::track(&track(id)).unwrap().id);
        clock
    };
    for (successor_at, successor_generation, successor_device, successor_item, should_pause) in [
        (2900, 1, "fixture-device", '4', true),
        (3900, 1, "fixture-device", '4', false),
        (2900, 2, "fixture-device", '4', false),
        (2900, 1, "another-device", '4', false),
        (2900, 1, "fixture-device", '3', false),
        (2900, 1, "fixture-device", '5', false),
    ] {
        let mut commands = owned_commands::OwnedCommands::default();
        let initial = track_clock(1, '1');
        for (index, control) in [
            RemoteOutputControl::Next,
            RemoteOutputControl::Next,
            RemoteOutputControl::Next,
            RemoteOutputControl::Pause,
        ]
        .into_iter()
        .enumerate()
        {
            commands
                .push(
                    control,
                    &initial,
                    now + Duration::from_millis(index as u64 * 20),
                )
                .unwrap();
        }
        for (sequence, id, elapsed) in [(1, '1', 90), (2, '2', 240), (3, '3', 400)] {
            let clock = track_clock(sequence, id);
            let Some(RemoteUpdate::OutputCommand(next)) =
                commands.poll(Some(&clock), now + Duration::from_millis(elapsed))
            else {
                panic!("Expected owned navigation {sequence}");
            };
            assert_eq!(next.control, RemoteOutputControl::Next);
            assert_eq!(Some(next.media_id), clock.media_id);
            commands.acknowledge(RemoteOutputCommandResult {
                id: next.id,
                generation: next.generation,
                accepted: true,
            });
        }
        let mut loading = track_clock(4, '3');
        loading.state = PlaybackState::Loading;
        loading.rate = 0.0;
        loading.actions = None;
        assert!(
            commands
                .poll(Some(&loading), now + Duration::from_millis(500))
                .is_none()
        );
        let stale = track_clock(3, '3');
        assert!(
            commands
                .poll(Some(&stale), now + Duration::from_millis(560))
                .is_none()
        );
        loading.sequence += 1;
        assert!(
            commands
                .poll(Some(&loading), now + Duration::from_millis(2400))
                .is_none()
        );
        let mut successor = track_clock(6, successor_item);
        successor.generation = successor_generation;
        successor.device_id = successor_device.into();
        let at = now + Duration::from_millis(successor_at);
        if successor_item == '5' {
            // An explicit source selection supersedes all queued navigations and Pause.
            let start = RemoteOutputControl::StartUri {
                uri: successor.media_id.as_ref().unwrap().as_str().into(),
                operation_id: 1,
            };
            commands.push(start.clone(), &successor, at).unwrap();
            assert!(
                matches!(commands.poll(Some(&successor), at), Some(RemoteUpdate::OutputCommand(command)) if command.control == start)
            );
            continue;
        }
        let update = commands.poll(Some(&successor), at);
        if should_pause {
            let Some(RemoteUpdate::OutputCommand(pause)) = update else {
                panic!("Expected Pause after all three navigations");
            };
            assert_eq!(pause.control, RemoteOutputControl::Pause);
            assert_eq!(Some(pause.media_id), successor.media_id);
        } else {
            assert!(matches!(
                update,
                Some(RemoteUpdate::OutputCommandFailed {
                    control: RemoteOutputControl::Pause,
                    ..
                })
            ));
        }
        assert!(commands.poll(Some(&successor), at).is_none());
    }
}

#[test]
fn local_dispatch_rejects_late_receipts_and_drops_followup_after_timeout() {
    let mut commands = owned_commands::OwnedCommands::default();
    let now = Instant::now();
    let clock = controllable_clock(1);
    commands
        .push(RemoteOutputControl::Next, &clock, now)
        .unwrap();
    let Some(RemoteUpdate::OutputCommand(sent)) = commands.poll(Some(&clock), now) else {
        panic!("Expected Next")
    };
    commands
        .push(RemoteOutputControl::Seek { position_ms: 1000 }, &clock, now)
        .unwrap();
    commands
        .push(RemoteOutputControl::Seek { position_ms: 4200 }, &clock, now)
        .unwrap();
    commands
        .push(RemoteOutputControl::Next, &clock, now)
        .unwrap();
    let wrong = RemoteOutputCommandResult {
        id: sent.id,
        generation: sent.generation + 1,
        accepted: true,
    };
    assert!(commands.acknowledge(wrong).is_none());
    assert!(commands.poll(Some(&clock), now).is_none());
    assert!(matches!(
        commands.poll(Some(&clock), now + Duration::from_secs(3)),
        Some(RemoteUpdate::OutputCommandFailed {
            control: RemoteOutputControl::Next,
            ..
        })
    ));
    let late = RemoteOutputCommandResult {
        id: sent.id,
        generation: sent.generation,
        accepted: true,
    };
    assert!(commands.acknowledge(late).is_none());
    assert!(
        commands
            .poll(Some(&clock), now + Duration::from_secs(3))
            .is_none()
    );
}

#[test]
fn volume_coalesces_and_new_recording_cannot_inherit_a_seek() {
    let mut commands = owned_commands::OwnedCommands::default();
    let now = Instant::now();
    let mut clock = controllable_clock(1);
    for percent in 0..=100 {
        commands
            .push(RemoteOutputControl::Volume { percent }, &clock, now)
            .unwrap();
    }
    let Some(RemoteUpdate::OutputCommand(sent)) = commands.poll(Some(&clock), now) else {
        panic!("Expected latest volume")
    };
    assert_eq!(sent.control, RemoteOutputControl::Volume { percent: 100 });
    assert_eq!(
        commands.acknowledge(RemoteOutputCommandResult {
            id: sent.id,
            generation: sent.generation,
            accepted: true
        }),
        Some(sent.control)
    );
    commands
        .push(RemoteOutputControl::Seek { position_ms: 4200 }, &clock, now)
        .unwrap();
    clock.media_id = Some(MediaId::parse(format!("spotify:track:{}", "9".repeat(22))).unwrap());
    assert!(matches!(
        commands.poll(Some(&clock), now),
        Some(RemoteUpdate::OutputCommandFailed {
            control: RemoteOutputControl::Seek { .. },
            ..
        })
    ));
}

#[test]
fn repeat_replacement_stays_on_the_owned_lane_through_an_action_refresh() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut initial = controllable_clock(1);
    initial.actions.as_mut().unwrap().repeat = true;
    initial.queue_epoch = Some(4);
    let observe = |provider: &mut SpotifyRemoteProvider, clock: RemoteOutputClock| {
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        wait(
            provider,
            |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
        );
    };
    let repeat = |mode| RemoteAction::Control {
        control: RemoteControl::Repeat { mode },
    };
    observe(&mut provider, initial.clone());
    provider.submit(repeat(RepeatMode::All)).unwrap();
    let RemoteUpdate::OutputCommand(first) = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(
        first.control,
        RemoteOutputControl::Repeat {
            mode: RepeatMode::All
        }
    );
    provider.submit(repeat(RepeatMode::One)).unwrap();
    let mut actionless = initial.clone();
    actionless.sequence += 1;
    actionless.actions = None;
    observe(&mut provider, actionless.clone());
    provider
        .submit(RemoteAction::OutputCommandResult {
            result: RemoteOutputCommandResult {
                id: first.id,
                generation: first.generation,
                accepted: true,
            },
        })
        .unwrap();
    provider.submit(repeat(RepeatMode::Off)).unwrap();
    while let Some(update) = provider.poll() {
        assert!(!matches!(
            update,
            RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. }
        ));
    }
    let mut refreshed = initial;
    refreshed.sequence = actionless.sequence + 1;
    observe(&mut provider, refreshed);
    let RemoteUpdate::OutputCommand(last) = wait(&mut provider, |update| {
        matches!(update, RemoteUpdate::OutputCommand(_))
    }) else {
        unreachable!()
    };
    assert_eq!(
        last.control,
        RemoteOutputControl::Repeat {
            mode: RepeatMode::Off
        }
    );
    assert_eq!(last.sequence, actionless.sequence + 1);
    assert!(
        server
            .records()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[test]
fn queued_shuffle_waits_only_for_missing_actions_on_its_original_transport() {
    let now = Instant::now();
    let mut complete = controllable_clock(1);
    complete.actions.as_mut().unwrap().shuffle = true;
    complete.queue_epoch = Some(4);
    let mut actionless = complete.clone();
    actionless.sequence += 1;
    actionless.actions = None;
    let control = RemoteOutputControl::Shuffle { enabled: true };
    for boundary in [
        "same",
        "new track",
        "new queue",
        "new device",
        "new generation",
        "denied",
        "expired",
        "revoked",
    ] {
        let mut commands = owned_commands::OwnedCommands::default();
        commands.push(control.clone(), &complete, now).unwrap();
        assert!(
            commands
                .poll(Some(&actionless), now + Duration::from_millis(20))
                .is_none()
        );
        let mut refreshed = complete.clone();
        refreshed.sequence = actionless.sequence + 1;
        let mut elapsed = Duration::from_millis(40);
        match boundary {
            "same" => (),
            "new track" => refreshed.media_id = Some(mapping::track(&track('9')).unwrap().id),
            "new queue" => refreshed.queue_epoch = Some(5),
            "new device" => refreshed.device_id = "another-device".into(),
            "new generation" => refreshed.generation += 1,
            "denied" => refreshed.actions.as_mut().unwrap().shuffle = false,
            "expired" => elapsed = Duration::from_secs(2),
            "revoked" => commands.revoke_output_transport(),
            _ => unreachable!(),
        }
        let result = commands.poll(Some(&refreshed), now + elapsed);
        if boundary == "same" {
            assert!(
                matches!(result, Some(RemoteUpdate::OutputCommand(command)) if command.control == control)
            );
        } else if boundary == "revoked" {
            assert!(result.is_none());
        } else {
            assert!(
                matches!(result, Some(RemoteUpdate::OutputCommandFailed { control: failed, .. }) if failed == control),
                "{boundary}"
            );
        }
    }
}

#[test]
fn repeat_then_seek_uses_the_owned_lane_through_the_confirmed_action_refresh() {
    for admission in ["queued", "after receipt"] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.queue_epoch = Some(9);
        clock.actions.as_mut().unwrap().repeat = true;
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let submit = |provider: &mut SpotifyRemoteProvider, control| {
            provider.submit(RemoteAction::Control { control }).unwrap();
        };
        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        submit(
            &mut provider,
            RemoteControl::Repeat {
                mode: RepeatMode::Off,
            },
        );
        if admission == "queued" {
            submit(&mut provider, RemoteControl::Seek { position_ms: 7500 });
        }
        let RemoteUpdate::OutputCommand(repeat) = wait(&mut provider, |event| {
            matches!(event, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert_eq!(
            repeat.control,
            RemoteOutputControl::Repeat {
                mode: RepeatMode::Off
            }
        );
        provider
            .submit(RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id: repeat.id,
                    generation: repeat.generation,
                    accepted: true,
                },
            })
            .unwrap();
        if admission == "queued" {
            // The last complete sample predates the setting and cannot be used.
            for _ in 0..3 {
                assert!(!matches!(
                    provider.poll(),
                    Some(RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. })
                ));
            }
        }
        let actions = clock.actions.take();
        clock.sequence += 1;
        observe(&mut provider, &clock);
        if admission == "after receipt" {
            submit(&mut provider, RemoteControl::Seek { position_ms: 7500 });
        }
        for _ in 0..3 {
            assert!(!matches!(
                provider.poll(),
                Some(RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. })
            ));
        }
        clock.sequence += 1;
        clock.actions = actions;
        observe(&mut provider, &clock);
        let RemoteUpdate::OutputCommand(seek) = wait(&mut provider, |event| {
            assert!(!matches!(event, RemoteUpdate::OutputCommandFailed { .. }));
            matches!(event, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert_eq!(
            seek.control,
            RemoteOutputControl::Seek { position_ms: 7500 }
        );
        assert_eq!(seek.sequence, clock.sequence);
        assert!(server.records().iter().all(|entry| entry.method == "GET"));
    }
}

#[test]
fn seek_behind_acknowledged_shuffle_rejects_expiry_denial_and_lost_occurrence() {
    for condition in [
        "denied",
        "expired",
        "no epoch",
        "new epoch",
        "device",
        "media",
        "generation",
        "missing clock",
    ] {
        let mut commands = owned_commands::OwnedCommands::default();
        let now = Instant::now();
        let mut clock = controllable_clock(1);
        clock.queue_epoch = (condition != "no epoch").then_some(9);
        clock.actions.as_mut().unwrap().shuffle = true;
        let seek = RemoteOutputControl::Seek { position_ms: 7500 };
        commands
            .push(RemoteOutputControl::Shuffle { enabled: true }, &clock, now)
            .unwrap();
        commands.push(seek.clone(), &clock, now).unwrap();
        let Some(RemoteUpdate::OutputCommand(setting)) = commands.poll(Some(&clock), now) else {
            unreachable!()
        };
        commands.acknowledge(RemoteOutputCommandResult {
            id: setting.id,
            generation: setting.generation,
            accepted: true,
        });
        let mut actionless = clock.clone();
        actionless.actions = None;
        actionless.sequence += 1;
        if condition != "no epoch" {
            assert!(
                commands
                    .poll(Some(&actionless), now + Duration::from_millis(30))
                    .is_none()
            );
        }
        clock.sequence += 2;
        let mut elapsed = Duration::from_millis(60);
        match condition {
            "denied" => clock.actions.as_mut().unwrap().seek = false,
            "expired" => elapsed = Duration::from_secs(2),
            "new epoch" => clock.queue_epoch = Some(10),
            "device" => clock.device_id = "other-device".into(),
            "media" => clock.media_id = Some(mapping::track(&track('2')).unwrap().id),
            "generation" => clock.generation += 1,
            "no epoch" | "missing clock" => (),
            _ => unreachable!(),
        }
        let followup = match condition {
            "no epoch" => Some(&actionless),
            "missing clock" => None,
            _ => Some(&clock),
        };
        assert!(
            matches!(commands.poll(followup, now + elapsed), Some(RemoteUpdate::OutputCommandFailed { control, .. }) if control == seek),
            "{condition}"
        );
        assert!(
            commands.poll(Some(&clock), now + elapsed).is_none(),
            "{condition}"
        );
    }
}

#[test]
fn http_owned_skip_burst_waits_for_each_successor_then_pauses_exact_device_once() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut initial = controllable_clock(1);
    initial.actions = None; // A stable owned clock without actions uses the Web API.
    let observe = |provider: &mut SpotifyRemoteProvider, clock| {
        provider
            .submit(RemoteAction::OutputClock { clock })
            .unwrap();
    };
    let control = |provider: &mut SpotifyRemoteProvider, control| {
        provider.submit(RemoteAction::Control { control }).unwrap();
    };
    let writes = || {
        server
            .records()
            .into_iter()
            .filter(|request| request.method != "GET")
            .map(|request| request.target)
            .collect::<Vec<_>>()
    };
    let wait_for = |count| {
        let until = Instant::now() + Duration::from_secs(3);
        while writes().len() < count {
            assert!(
                Instant::now() < until,
                "Expected {count} transport writes, got {:?}",
                writes()
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    observe(&mut provider, Some(initial.clone()));
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    server.fail_playback_reads.store(true, Ordering::SeqCst);
    control(&mut provider, RemoteControl::Next);
    control(&mut provider, RemoteControl::Next);
    control(&mut provider, RemoteControl::Pause);
    wait_for(1);
    let mut successor = initial.clone();
    successor.sequence = 2;
    observe(&mut provider, Some(successor.clone()));
    thread::sleep(Duration::from_millis(125));
    assert_eq!(
        writes().len(),
        1,
        "Old track clock must not release the second Next"
    );
    successor.sequence = 3;
    successor.media_id = Some(mapping::track(&track('2')).unwrap().id);
    observe(&mut provider, Some(successor.clone()));
    wait_for(2);
    observe(&mut provider, None);
    thread::sleep(Duration::from_millis(125));
    assert_eq!(writes().len(), 2, "A missing clock must not release Pause");
    successor.sequence = 4;
    successor.state = PlaybackState::Loading;
    successor.rate = 0.0;
    successor.media_id = Some(mapping::track(&track('3')).unwrap().id);
    observe(&mut provider, Some(successor.clone()));
    thread::sleep(Duration::from_millis(125));
    assert_eq!(writes().len(), 2, "Loading must not release Pause");
    let mut stale = successor.clone();
    stale.sequence = 3;
    stale.state = PlaybackState::Playing;
    stale.rate = 1.0;
    observe(&mut provider, Some(stale));
    thread::sleep(Duration::from_millis(125));
    assert_eq!(
        writes().len(),
        2,
        "An out-of-order positive clock must not release Pause"
    );
    successor.sequence = 5;
    successor.state = PlaybackState::Playing;
    successor.rate = 1.0;
    observe(&mut provider, Some(successor));
    wait_for(3);
    assert_eq!(
        writes(),
        [
            "/v1/me/player/next?device_id=fixture-device",
            "/v1/me/player/next?device_id=fixture-device",
            "/v1/me/player/pause?device_id=fixture-device",
        ]
    );
}

#[test]
fn ready_scoped_pause_precedes_the_slow_playback_read_and_polling_resumes() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = controllable_clock(1);
    clock.actions = None;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update, RemoteUpdate::State(state) if state.owned_output_control),
    );

    server.hold_next_response.store(true, Ordering::SeqCst);
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let first_next = loop {
        if let Some(index) = server
            .records()
            .iter()
            .position(|request| request.method == "POST")
        {
            break index;
        }
        assert!(Instant::now() < until, "Expected the first scoped write");
        thread::sleep(Duration::from_millis(5));
    };
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Pause,
        })
        .unwrap();
    clock.sequence += 1;
    clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
    provider
        .submit(RemoteAction::OutputClock { clock: Some(clock) })
        .unwrap();

    server.hold_playback.store(true, Ordering::SeqCst);
    server.hold_next_response.store(false, Ordering::SeqCst);
    let until = Instant::now() + Duration::from_secs(2);
    let transport = loop {
        let records = server.records();
        let transport = records[first_next..]
            .iter()
            .filter(|request| {
                request.method != "GET" || request.target.starts_with("/v1/me/player?")
            })
            .map(|request| (request.method.clone(), request.target.clone()))
            .collect::<Vec<_>>();
        if transport.iter().any(|(method, _)| method == "GET") {
            break transport;
        }
        assert!(Instant::now() < until, "Playback polling did not resume");
        thread::sleep(Duration::from_millis(5));
    };
    server.hold_playback.store(false, Ordering::SeqCst);
    assert_eq!(transport.len(), 3);
    assert_eq!(transport[0].0, "POST");
    assert_eq!(
        transport[0].1,
        "/v1/me/player/next?device_id=fixture-device"
    );
    assert_eq!(transport[1].0, "PUT");
    assert_eq!(
        transport[1].1,
        "/v1/me/player/pause?device_id=fixture-device"
    );
    assert_eq!(transport[2].0, "GET");
}

#[test]
fn http_owned_skip_drops_its_pause_when_device_or_runtime_changes() {
    for replace_device in [false, true] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.actions = None;
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
        );
        for control in [RemoteControl::Next, RemoteControl::Pause] {
            provider.submit(RemoteAction::Control { control }).unwrap();
        }
        let until = Instant::now() + Duration::from_secs(2);
        while !server
            .records()
            .iter()
            .any(|request| request.method == "POST")
        {
            assert!(Instant::now() < until, "Expected the admitted Next write");
            thread::sleep(Duration::from_millis(5));
        }
        clock.sequence = 2;
        clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
        if replace_device {
            clock.device_id = "different-device".into();
        } else {
            clock.generation += 1;
        }
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        thread::sleep(Duration::from_millis(125));
        let writes = server
            .records()
            .into_iter()
            .filter(|request| request.method != "GET")
            .map(|request| request.target)
            .collect::<Vec<_>>();
        assert_eq!(writes, ["/v1/me/player/next?device_id=fixture-device"]);
    }
}

#[test]
fn stable_owned_previous_sends_one_exact_http_post_at_any_explicit_position() {
    for (position_ms, state) in [
        (100, PlaybackState::Paused),
        (2770, PlaybackState::Playing),
        (3500, PlaybackState::Paused),
        (4050, PlaybackState::Playing),
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.state = state;
        clock.position_ms = position_ms;
        if state == PlaybackState::Paused {
            clock.rate = 0.0;
        }
        clock.actions.as_mut().unwrap().previous = true;
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
        );
        provider.submit_previous_with_request_id(72).unwrap();
        if state == PlaybackState::Paused {
            assert!(provider.has_pending_output_start());
        }
        let until = Instant::now() + Duration::from_secs(2);
        while !server
            .records()
            .iter()
            .any(|request| request.method == "POST")
        {
            assert!(
                Instant::now() < until,
                "Previous did not write at {position_ms}ms"
            );
            assert!(!matches!(
                provider.poll(),
                Some(RemoteUpdate::OutputCommand(_))
            ));
            thread::sleep(Duration::from_millis(5));
        }
        clock.sequence = 2;
        clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while provider.has_pending_output_start() {
            assert!(Instant::now() < until);
            assert!(!matches!(
                provider.poll(),
                Some(RemoteUpdate::OutputCommand(_))
            ));
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!matches!(
            provider.poll(),
            Some(RemoteUpdate::OutputCommand(_))
        ));
        assert_eq!(
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>(),
            ["/v1/me/player/previous?device_id=fixture-device"]
        );
    }
}

#[test]
fn owned_previous_is_cancelled_if_its_media_changes_before_the_http_write() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let mut clock = controllable_clock(1);
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.position_ms = 3500;
    clock.actions.as_mut().unwrap().previous = true;
    provider
        .submit(RemoteAction::OutputClock {
            clock: Some(clock.clone()),
        })
        .unwrap();
    wait(
        &mut provider,
        |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
    );
    let reads = || {
        server
            .records()
            .iter()
            .filter(|request| request.target.starts_with("/v1/me/player?"))
            .count()
    };
    let before = reads();
    server.hold_playback.store(true, Ordering::SeqCst);
    provider.submit(RemoteAction::Refresh).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while reads() == before {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Previous,
        })
        .unwrap();
    assert!(provider.has_pending_output_start());
    clock.sequence = 2;
    clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
    provider
        .submit(RemoteAction::OutputClock { clock: Some(clock) })
        .unwrap();
    assert!(!provider.has_pending_output_start());
    server.hold_playback.store(false, Ordering::SeqCst);
    wait(&mut provider, |u| {
        assert!(!matches!(u, RemoteUpdate::OutputCommand(_)));
        matches!(u, RemoteUpdate::State(s) if s.issue.as_ref().is_some_and(|issue| issue.code == ErrorCode::InvalidCommand))
    });
    assert!(
        server
            .records()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[test]
fn accepted_play_releases_only_the_exact_actionful_loading_followup() {
    for (control, loading_first) in [
        (RemoteOutputControl::Next, true),
        (RemoteOutputControl::Next, false),
        (RemoteOutputControl::Pause, true),
        (RemoteOutputControl::Pause, false),
    ] {
        let now = Instant::now();
        let mut paused = controllable_clock(1);
        paused.state = PlaybackState::Paused;
        paused.rate = 0.0;
        paused.queue_epoch = Some(7);
        paused.actions.as_mut().unwrap().play = true;
        let mut commands = owned_commands::OwnedCommands::default();
        commands
            .push(RemoteOutputControl::Play, &paused, now)
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(play)) = commands.poll(Some(&paused), now) else {
            panic!("Expected Play");
        };
        let at = now + Duration::from_millis(60);
        let mut loading = paused.clone();
        loading.sequence += 1;
        loading.state = PlaybackState::Loading;
        let request_id = (control == RemoteOutputControl::Next).then_some(73);
        commands
            .push_with_request_id(
                control.clone(),
                if loading_first { &loading } else { &paused },
                at,
                request_id,
            )
            .unwrap();
        let before_ack = if loading_first { &loading } else { &paused };
        assert!(!commands.owned_followup_ready(before_ack, at));
        assert!(commands.poll(Some(before_ack), at).is_none());
        commands.acknowledge(RemoteOutputCommandResult {
            id: play.id + 1,
            generation: play.generation,
            accepted: true,
        });
        assert!(!commands.owned_followup_ready(&loading, at));
        commands.acknowledge(RemoteOutputCommandResult {
            id: play.id,
            generation: play.generation,
            accepted: true,
        });
        assert!(!commands.owned_followup_ready(&paused, at));
        assert!(commands.poll(Some(&paused), at).is_none());

        for exclusion in [
            "actions",
            "denied",
            "epoch",
            "missing epoch",
            "sequence",
            "media",
            "device",
            "generation",
            "inactive",
        ] {
            let mut unmatched = loading.clone();
            match exclusion {
                "actions" => unmatched.actions = None,
                "denied" => {
                    unmatched.actions.as_mut().unwrap().next = false;
                    unmatched.actions.as_mut().unwrap().pause = false;
                }
                "epoch" => unmatched.queue_epoch = Some(8),
                "missing epoch" => unmatched.queue_epoch = None,
                "sequence" => unmatched.sequence = play.sequence,
                "media" => unmatched.media_id = Some(mapping::track(&track('2')).unwrap().id),
                "device" => unmatched.device_id = "another-device".into(),
                "generation" => unmatched.generation += 1,
                "inactive" => unmatched.active = false,
                _ => unreachable!(),
            }
            assert!(
                !commands.owned_followup_ready(&unmatched, at),
                "{exclusion}"
            );
            if matches!(exclusion, "actions" | "denied" | "sequence") {
                assert!(commands.poll(Some(&unmatched), at).is_none(), "{exclusion}");
            }
        }
        assert!(commands.owned_followup_ready(&loading, at));
        let Some(RemoteUpdate::OutputCommand(sent)) = commands.poll(Some(&loading), at) else {
            panic!("The exact Loading clock should release {control:?} without the settle timer");
        };
        assert_eq!(sent.control, control);
        assert_eq!(sent.request_id, request_id);
        assert_eq!(sent.required_queue_epoch, Some(7));
        assert_eq!(sent.sequence, loading.sequence);
        assert_eq!(Some(sent.media_id), loading.media_id);
        assert!(!commands.owned_followup_ready(&loading, at));
        assert!(commands.poll(Some(&loading), at).is_none());
    }
}

#[test]
fn late_play_cannot_consume_its_trailing_pause_on_the_preceding_paused_clock() {
    let now = Instant::now();
    let mut paused = controllable_clock(1);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    paused.queue_epoch = Some(7);
    paused.actions.as_mut().unwrap().play = true;
    let mut commands = owned_commands::OwnedCommands::default();
    commands
        .push(RemoteOutputControl::Play, &paused, now)
        .unwrap();
    let Some(RemoteUpdate::OutputCommand(play)) = commands.poll(Some(&paused), now) else {
        panic!("Expected Play");
    };
    commands
        .push(RemoteOutputControl::Pause, &paused, now)
        .unwrap();
    commands.acknowledge(RemoteOutputCommandResult {
        id: play.id,
        generation: play.generation,
        accepted: true,
    });
    let at = now + Duration::from_millis(535);
    assert!(commands.poll(Some(&paused), at).is_none());
    assert!(commands.next_is_output_transport());
    let mut playing = paused.clone();
    playing.sequence += 1;
    playing.state = PlaybackState::Playing;
    playing.rate = 1.0;
    let Some(RemoteUpdate::OutputCommand(pause)) = commands.poll(Some(&playing), at) else {
        panic!("Expected Pause when the delayed Play starts");
    };
    assert_eq!(pause.control, RemoteOutputControl::Pause);
    assert_eq!(pause.required_queue_epoch, None);
}

#[test]
fn a_rejected_loading_next_keeps_only_the_bound_successor_pause() {
    for successor_already_observed in [false, true] {
        let now = Instant::now();
        let at = now + Duration::from_millis(50);
        let mut paused = controllable_clock(1);
        paused.state = PlaybackState::Paused;
        paused.rate = 0.0;
        paused.queue_epoch = Some(7);
        paused.actions.as_mut().unwrap().play = true;
        let mut commands = owned_commands::OwnedCommands::default();
        commands
            .push(RemoteOutputControl::Play, &paused, now)
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(play)) = commands.poll(Some(&paused), now) else {
            panic!("Expected Play");
        };
        commands.acknowledge(RemoteOutputCommandResult {
            id: play.id,
            generation: play.generation,
            accepted: true,
        });
        let mut loading = paused;
        loading.sequence += 1;
        loading.state = PlaybackState::Loading;
        commands
            .push(RemoteOutputControl::Next, &loading, at)
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(next)) = commands.poll(Some(&loading), at) else {
            panic!("Expected scoped early Next");
        };
        assert_eq!(next.required_queue_epoch, Some(7));

        let mut successor = loading.clone();
        successor.queue_epoch = Some(8);
        successor.sequence += 1;
        successor.media_id = Some(mapping::track(&track('2')).unwrap().id);
        commands
            .push(
                RemoteOutputControl::Pause,
                if successor_already_observed {
                    &successor
                } else {
                    &loading
                },
                at,
            )
            .unwrap();
        commands.acknowledge(RemoteOutputCommandResult {
            id: next.id,
            generation: next.generation,
            accepted: false,
        });
        assert!(!commands.owned_followup_ready(&loading, at));
        assert!(commands.poll(Some(&loading), at).is_none());
        assert!(!commands.owned_followup_ready(&successor, at));
        assert!(commands.poll(Some(&successor), at).is_none());
        successor.state = PlaybackState::Playing;
        successor.rate = 1.0;
        successor.sequence += 1;
        successor.position_ms = 81_273;
        assert!(!commands.owned_followup_ready(&successor, at));
        assert!(commands.poll(Some(&successor), at).is_none());
        successor.sequence += 1;
        successor.position_ms = 1;
        successor.actions.as_mut().unwrap().pause = false;
        assert!(!commands.owned_followup_ready(&successor, at));
        assert!(commands.poll(Some(&successor), at).is_none());
        successor.actions.as_mut().unwrap().pause = true;
        assert!(commands.owned_followup_ready(&successor, at));
        let Some(RemoteUpdate::OutputCommand(pause)) = commands.poll(Some(&successor), at) else {
            panic!("Expected only the trailing Pause on the confirmed successor");
        };
        assert_eq!(pause.control, RemoteOutputControl::Pause);
        assert_eq!(pause.required_queue_epoch, Some(8));
        assert_eq!(Some(pause.media_id), successor.media_id);
        assert!(commands.poll(Some(&successor), at).is_none());
    }
}

#[test]
fn seek_queued_during_owned_play_waits_for_new_same_item_actions() {
    for (replace_item, expire, allow_seek) in [
        (false, false, true),
        (true, false, true),
        (false, true, true),
        (false, false, false),
    ] {
        let mut commands = owned_commands::OwnedCommands::default();
        let now = Instant::now();
        let mut paused = controllable_clock(1);
        paused.state = PlaybackState::Paused;
        paused.rate = 0.0;
        paused.actions.as_mut().unwrap().play = true;
        let seek = RemoteOutputControl::Seek { position_ms: 6500 };
        commands
            .push(RemoteOutputControl::Play, &paused, now)
            .unwrap();
        let Some(RemoteUpdate::OutputCommand(play)) = commands.poll(Some(&paused), now) else {
            panic!("Expected Play");
        };
        commands
            .push(seek.clone(), &paused, now + Duration::from_millis(190))
            .unwrap();
        commands.acknowledge(RemoteOutputCommandResult {
            id: play.id,
            generation: play.generation,
            accepted: true,
        });
        assert!(
            commands
                .poll(Some(&paused), now + Duration::from_millis(1050))
                .is_none()
        );
        assert!(
            commands
                .poll(None, now + Duration::from_millis(1075))
                .is_none()
        );
        let mut playing = controllable_clock(2);
        let actions = playing.actions.take();
        assert!(
            commands
                .poll(Some(&playing), now + Duration::from_millis(1100))
                .is_none()
        );
        playing.sequence = 3;
        playing.actions = actions;
        playing.actions.as_mut().unwrap().seek = allow_seek;
        if replace_item {
            playing.media_id = Some(mapping::track(&track('2')).unwrap().id);
        }
        let ready_at = now + Duration::from_millis(if expire { 2190 } else { 1150 });
        let result = commands.poll(Some(&playing), ready_at);
        if replace_item || expire || !allow_seek {
            assert!(
                matches!(result, Some(RemoteUpdate::OutputCommandFailed { control, .. }) if control == seek)
            );
        } else {
            let Some(RemoteUpdate::OutputCommand(sent)) = result else {
                panic!("Expected the queued seek");
            };
            assert_eq!(sent.control, seek);
            assert_eq!(sent.sequence, playing.sequence);
            assert_eq!(Some(sent.media_id), playing.media_id);
        }
        assert!(commands.poll(Some(&playing), ready_at).is_none());
    }
}

#[test]
fn an_owned_seek_on_stable_paused_playback_dispatches_without_waiting_for_play() {
    let mut commands = owned_commands::OwnedCommands::default();
    let now = Instant::now();
    let mut paused = controllable_clock(1);
    paused.state = PlaybackState::Paused;
    paused.rate = 0.0;
    let seek = RemoteOutputControl::Seek { position_ms: 6500 };
    commands.push(seek.clone(), &paused, now).unwrap();
    assert!(
        matches!(commands.poll(Some(&paused), now), Some(RemoteUpdate::OutputCommand(sent)) if sent.control == seek)
    );
}

#[test]
fn exact_previous_waits_for_the_owned_seek_before_its_ordered_followups() {
    for order in [
        "queued",
        "clock first",
        "receipt first",
        "already acknowledged",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.queue_epoch = Some(1);
        clock.position_ms = 6500;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.actions.as_mut().unwrap().previous = true;
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let submit = |provider: &mut SpotifyRemoteProvider, control| {
            provider.submit(RemoteAction::Control { control }).unwrap();
        };
        let dispatch = |provider: &mut SpotifyRemoteProvider| {
            let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
                matches!(event, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            assert_eq!(
                command.control,
                RemoteOutputControl::Seek { position_ms: 3500 }
            );
            command
        };
        let acknowledge =
            |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand, id| {
                provider
                    .submit(RemoteAction::OutputCommandResult {
                        result: RemoteOutputCommandResult {
                            id,
                            generation: command.generation,
                            accepted: true,
                        },
                    })
                    .unwrap();
            };
        let writes = || {
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>()
        };
        let wait_for = |count| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while writes().len() < count {
                assert!(Instant::now() < deadline, "{order}: {:?}", writes());
                thread::sleep(Duration::from_millis(5));
            }
        };
        let waiting_for_seek = |provider: &SpotifyRemoteProvider| {
            provider
                .shared
                .0
                .lock()
                .unwrap()
                .front_owned_http()
                .is_some_and(|lane| lane.waiting_for_seek().is_some())
        };

        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        submit(&mut provider, RemoteControl::Seek { position_ms: 3500 });
        let command = (order != "queued").then(|| dispatch(&mut provider));
        if order == "already acknowledged" {
            let command = command.as_ref().unwrap();
            acknowledge(&mut provider, command, command.id);
        }
        for control in [
            RemoteControl::Previous,
            RemoteControl::Next,
            RemoteControl::Pause,
        ] {
            submit(&mut provider, control);
        }
        assert!(provider.has_pending_output_start());
        let command = command.unwrap_or_else(|| dispatch(&mut provider));
        assert!(waiting_for_seek(&provider), "{order}");

        // A later sequence alone is not evidence for the applied seek.
        clock.sequence += 1;
        observe(&mut provider, &clock);
        if order == "receipt first" {
            acknowledge(&mut provider, &command, command.id);
        }
        assert!(waiting_for_seek(&provider), "{order}");
        clock.position_ms = 3500;
        clock.sequence += 1;
        clock.actions.as_mut().unwrap().previous = false;
        observe(&mut provider, &clock);
        if matches!(order, "queued" | "clock first") {
            acknowledge(&mut provider, &command, command.id + 10);
            assert!(waiting_for_seek(&provider), "{order}");
            assert!(writes().is_empty());
        }
        assert!(waiting_for_seek(&provider), "{order}");
        clock.actions.as_mut().unwrap().previous = true;
        clock.sequence += 1;
        observe(&mut provider, &clock);
        if matches!(order, "queued" | "clock first") {
            assert!(waiting_for_seek(&provider), "{order}");
            acknowledge(&mut provider, &command, command.id);
        }
        wait_for(1);
        assert_eq!(
            writes(),
            ["/v1/me/player/previous?device_id=fixture-device"]
        );

        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.position_ms = 0;
        clock.sequence += 1;
        clock.queue_epoch = Some(2);
        clock.media_id = Some(mapping::track(&track('2')).unwrap().id);
        observe(&mut provider, &clock);
        wait_for(2);
        assert_eq!(
            writes().len(),
            2,
            "Pause cannot overtake the second transition"
        );
        clock.sequence += 1;
        clock.queue_epoch = Some(3);
        clock.media_id = Some(mapping::track(&track('3')).unwrap().id);
        observe(&mut provider, &clock);
        wait_for(3);
        assert_eq!(
            writes(),
            [
                "/v1/me/player/previous?device_id=fixture-device",
                "/v1/me/player/next?device_id=fixture-device",
                "/v1/me/player/pause?device_id=fixture-device",
            ]
        );
    }
}

#[test]
fn previous_can_only_reserve_the_latest_owned_seek_within_its_original_deadline() {
    let mut commands = owned_commands::OwnedCommands::default();
    let queued_at = Instant::now();
    let mut clock = controllable_clock(1);
    clock.actions.as_mut().unwrap().previous = true;
    commands
        .push(
            RemoteOutputControl::Seek { position_ms: 3000 },
            &clock,
            queued_at,
        )
        .unwrap();
    let older = commands.seek_before_previous(&clock, queued_at).unwrap();
    let updated = queued_at + Duration::from_millis(500);
    commands
        .push(
            RemoteOutputControl::Seek { position_ms: 3500 },
            &clock,
            updated,
        )
        .unwrap();
    let barrier = commands.seek_before_previous(&clock, updated).unwrap();
    assert!(!commands.waits_for_seek(older, updated));
    let Some(RemoteUpdate::OutputCommand(command)) = commands.poll(Some(&clock), updated) else {
        unreachable!()
    };
    assert_eq!(
        command.control,
        RemoteOutputControl::Seek { position_ms: 3500 }
    );
    commands.acknowledge(RemoteOutputCommandResult {
        id: command.id,
        generation: command.generation,
        accepted: true,
    });
    clock.sequence += 1;
    clock.position_ms = 3500;
    let last_moment = updated + Duration::from_millis(1999);
    assert!(commands.seek_confirmed(barrier, Some(&clock), last_moment));
    let expired = updated + Duration::from_secs(2);
    assert!(!commands.seek_confirmed(barrier, Some(&clock), expired));
    assert!(!commands.waits_for_seek(barrier, expired));
    assert!(commands.seek_before_previous(&clock, expired).is_none());
}

#[test]
fn failed_or_replaced_owned_seek_discards_the_waiting_web_previous() {
    for boundary in [
        "rejected",
        "new generation",
        "new occurrence",
        "no output",
        "transfer",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.queue_epoch = Some(1);
        clock.position_ms = 6500;
        clock.actions.as_mut().unwrap().previous = true;
        let observe = |provider: &mut SpotifyRemoteProvider, clock| {
            provider
                .submit(RemoteAction::OutputClock { clock })
                .unwrap();
        };
        observe(&mut provider, Some(clock.clone()));
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        for control in [
            RemoteControl::Seek { position_ms: 3500 },
            RemoteControl::Previous,
            RemoteControl::Pause,
        ] {
            provider.submit(RemoteAction::Control { control }).unwrap();
        }
        if boundary == "transfer" {
            // A new selection can replace an unsent seek and its blocked HTTP burst.
            provider
                .submit(RemoteAction::Control {
                    control: RemoteControl::Transfer {
                        device_id: "fixture-device".into(),
                        operation_id: Some(77),
                    },
                })
                .unwrap();
            wait(
                &mut provider,
                |event| matches!(event, RemoteUpdate::State(state) if state.control_result.as_ref().is_some_and(|result| result.operation_id == 77 && result.error.is_none())),
            );
        } else {
            let RemoteUpdate::OutputCommand(command) = wait(&mut provider, |event| {
                matches!(event, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            clock.sequence += 1;
            clock.position_ms = 3500;
            match boundary {
                "rejected" => observe(&mut provider, Some(clock)),
                "new generation" => {
                    clock.generation += 1;
                    observe(&mut provider, Some(clock));
                }
                "new occurrence" => {
                    clock.queue_epoch = Some(2);
                    observe(&mut provider, Some(clock));
                }
                "no output" => observe(&mut provider, None),
                _ => unreachable!(),
            }
            provider
                .submit(RemoteAction::OutputCommandResult {
                    result: RemoteOutputCommandResult {
                        id: command.id,
                        generation: command.generation,
                        accepted: boundary != "rejected",
                    },
                })
                .unwrap();
        }
        let mailbox = provider.shared.0.lock().unwrap();
        assert!(mailbox.owned_http.is_none(), "{boundary}");
        assert!(mailbox.controls.is_empty(), "{boundary}");
        assert!(
            server
                .records()
                .iter()
                .all(|request| request.method == "GET"
                    || boundary == "transfer" && request.target == "/v1/me/player?"),
            "{boundary}"
        );
    }
}

#[test]
fn exact_previous_promotes_one_scoped_pause_and_keeps_next_behind_its_receipt() {
    for (pre_ack_witness, interruption) in [
        (false, None),
        (true, None),
        (true, Some("replaced")),
        (false, Some("rejected")),
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        let context = format!("spotify:album:{}", "a".repeat(22));
        let successor = mapping::track(&track('2')).unwrap().id;
        clock.queue_epoch = Some(10);
        clock.actions.as_mut().unwrap().previous = true;
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Fixture track".into(),
            artists: vec!["Fixture artist".into()],
            album: None,
            duration_ms: 8000,
            artwork: None,
            context_uri: Some(context),
        });
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let submit = |provider: &mut SpotifyRemoteProvider, control| {
            provider.submit(RemoteAction::Control { control }).unwrap();
        };
        let writes = || {
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>()
        };
        let wait_for = |count| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while writes().len() < count {
                assert!(
                    Instant::now() < deadline,
                    "Expected {count} writes, got {:?}",
                    writes()
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        if pre_ack_witness {
            provider
                .submit(RemoteAction::OutputQueue {
                    queue: RemoteOutputQueue {
                        generation: clock.generation,
                        device_id: clock.device_id.clone(),
                        media_id: clock.media_id.clone().unwrap(),
                        epoch: 10,
                        sequence: 1,
                        autoplay_ready: false,
                        previous: vec![RemoteOutputQueueEntry {
                            media_id: successor.clone(),
                            source: Some(QueueEntrySource::Context),
                            metadata: None,
                        }],
                        upcoming: vec![],
                    },
                })
                .unwrap();
        }
        server.hold_previous_response.store(true, Ordering::SeqCst);
        submit(&mut provider, RemoteControl::Previous);
        wait_for(1);
        submit(&mut provider, RemoteControl::Pause);
        clock.position_ms = 0;
        clock.media_id = Some(successor.clone());
        clock.sequence += 1;
        clock.queue_epoch = Some(11);
        observe(&mut provider, &clock);

        if !pre_ack_witness {
            // Positive but uncorrelated changed media cannot run ahead of ACK.
            for _ in 0..10 {
                assert!(!matches!(
                    provider.poll(),
                    Some(RemoteUpdate::OutputCommand(_))
                ));
                thread::sleep(Duration::from_millis(5));
            }
            server.hold_previous_response.store(false, Ordering::SeqCst);
        }
        let RemoteUpdate::OutputCommand(pause) = wait(&mut provider, |event| {
            matches!(event, RemoteUpdate::OutputCommand(_))
        }) else {
            unreachable!()
        };
        assert_eq!(pause.control, RemoteOutputControl::Pause);
        assert_eq!(pause.required_queue_epoch, Some(11));
        assert_eq!(pause.media_id, successor);
        assert_eq!(pause.device_id, clock.device_id);
        submit(&mut provider, RemoteControl::Next);
        if let Some(interruption) = interruption {
            if interruption == "replaced" {
                // Even the same URI with another occurrence cannot inherit
                // the promoted Pause or its dependent Next.
                clock.queue_epoch = Some(12);
                clock.sequence += 1;
                observe(&mut provider, &clock);
            }
            provider
                .submit(RemoteAction::OutputCommandResult {
                    result: RemoteOutputCommandResult {
                        id: pause.id,
                        generation: pause.generation,
                        accepted: false,
                    },
                })
                .unwrap();
            server.hold_previous_response.store(false, Ordering::SeqCst);
            for _ in 0..10 {
                assert!(!matches!(
                    provider.poll(),
                    Some(RemoteUpdate::OutputCommand(_))
                ));
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                writes(),
                ["/v1/me/player/previous?device_id=fixture-device"]
            );
            continue;
        }
        let acknowledge = |provider: &mut SpotifyRemoteProvider, id| {
            provider
                .submit(RemoteAction::OutputCommandResult {
                    result: RemoteOutputCommandResult {
                        id,
                        generation: pause.generation,
                        accepted: true,
                    },
                })
                .unwrap();
        };
        if !pre_ack_witness {
            acknowledge(&mut provider, pause.id);
        }
        for _ in 0..5 {
            assert_eq!(
                writes(),
                ["/v1/me/player/previous?device_id=fixture-device"]
            );
            assert!(!matches!(
                provider.poll(),
                Some(RemoteUpdate::OutputCommand(_))
            ));
            thread::sleep(Duration::from_millis(5));
        }
        clock.sequence += 1;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        observe(&mut provider, &clock);
        if pre_ack_witness {
            acknowledge(&mut provider, pause.id + 10);
            assert_eq!(writes().len(), 1);
            acknowledge(&mut provider, pause.id);
            assert_eq!(
                writes().len(),
                1,
                "Must still wait for Previous's HTTP reply"
            );
            server.hold_previous_response.store(false, Ordering::SeqCst);
        }
        wait_for(2);
        assert_eq!(
            writes(),
            [
                "/v1/me/player/previous?device_id=fixture-device",
                "/v1/me/player/next?device_id=fixture-device",
            ]
        );
    }
}

#[test]
fn cued_previous_holds_scope_and_orders_an_original_seek_and_followups() {
    for (leading, outcome) in [
        ("queued", "next"),
        ("dispatched", "play"),
        ("none", "next"),
        ("none", "wrong track"),
        ("none", "rejected"),
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        let expected = mapping::track(&track('2')).unwrap().id;
        let context = format!("spotify:album:{}", "a".repeat(22));
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.position_ms = if leading == "none" { 3500 } else { 6500 };
        clock.queue_epoch = Some(10);
        clock.actions.as_mut().unwrap().previous = true;
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Fixture track".into(),
            artists: vec!["Fixture artist".into()],
            album: None,
            duration_ms: 8000,
            artwork: None,
            context_uri: Some(context.clone()),
        });
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let submit = |provider: &mut SpotifyRemoteProvider, control| {
            provider.submit(RemoteAction::Control { control }).unwrap();
        };
        let command = |provider: &mut SpotifyRemoteProvider| {
            let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
                matches!(event, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            command
        };
        let acknowledge =
            |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand, accepted| {
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
        let writes = || {
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>()
        };
        let expect_no_write = |provider: &mut SpotifyRemoteProvider| {
            for _ in 0..6 {
                assert!(!matches!(
                    provider.poll(),
                    Some(RemoteUpdate::OutputCommand(_))
                ));
                assert!(writes().is_empty(), "{leading}/{outcome}");
                thread::sleep(Duration::from_millis(3));
            }
        };
        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        let entry = RemoteOutputQueueEntry {
            media_id: expected.clone(),
            source: Some(QueueEntrySource::Context),
            metadata: None,
        };
        provider
            .submit(RemoteAction::OutputQueue {
                queue: RemoteOutputQueue {
                    generation: clock.generation,
                    device_id: clock.device_id.clone(),
                    media_id: clock.media_id.clone().unwrap(),
                    epoch: 10,
                    sequence: 1,
                    autoplay_ready: false,
                    previous: vec![entry.clone(), entry.clone()],
                    upcoming: vec![entry],
                },
            })
            .unwrap();
        if leading != "none" {
            submit(&mut provider, RemoteControl::Seek { position_ms: 3500 });
        }
        let sent_seek = (leading == "dispatched").then(|| command(&mut provider));
        submit(&mut provider, RemoteControl::Previous);
        submit(&mut provider, RemoteControl::Pause);
        if outcome == "next" {
            submit(&mut provider, RemoteControl::Next);
        }
        if leading != "none" {
            let seek = sent_seek.unwrap_or_else(|| command(&mut provider));
            assert_eq!(
                seek.control,
                RemoteOutputControl::Seek { position_ms: 3500 }
            );
            assert!(!provider.owned_handoff_ready());
            if outcome != "play" {
                acknowledge(&mut provider, &seek, true);
            }
            expect_no_write(&mut provider);
            assert!(!provider.owned_handoff_ready());
            clock.position_ms = 3500;
            clock.sequence += 1;
            observe(&mut provider, &clock);
            if outcome == "play" {
                assert!(!provider.owned_handoff_ready());
                acknowledge(&mut provider, &seek, true);
            }
        }

        assert!(provider.owned_handoff_ready(), "{leading}/{outcome}");
        let previous = command(&mut provider);
        assert_eq!(previous.control, RemoteOutputControl::PreviousFromStart);
        assert_eq!(previous.required_queue_epoch, Some(10));
        assert_eq!(Some(&previous.media_id), clock.media_id.as_ref());
        assert_eq!(previous.device_id, clock.device_id);
        assert!(!provider.owned_handoff_ready());
        assert!(writes().is_empty());
        clock.position_ms = 0;
        clock.sequence += 1;
        observe(&mut provider, &clock);
        if outcome == "rejected" {
            acknowledge(&mut provider, &previous, false);
            expect_no_write(&mut provider);
            continue;
        }
        if outcome != "play" {
            acknowledge(&mut provider, &previous, true);
        }
        // Neither the successful compound receipt nor its own cue may release Pause.
        expect_no_write(&mut provider);
        clock.media_id = Some(if outcome == "wrong track" {
            mapping::track(&track('3')).unwrap().id
        } else {
            expected.clone()
        });
        clock.queue_epoch = Some(11);
        clock.sequence += 1;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.metadata.as_mut().unwrap().context_uri = None;
        observe(&mut provider, &clock);
        expect_no_write(&mut provider);
        if outcome == "wrong track" {
            continue;
        }
        clock.metadata.as_mut().unwrap().context_uri = Some(context);
        clock.sequence += 1;
        observe(&mut provider, &clock);
        if outcome == "play" {
            expect_no_write(&mut provider);
            acknowledge(&mut provider, &previous, true);
        }

        let pause = command(&mut provider);
        assert_eq!(pause.control, RemoteOutputControl::Pause);
        assert_eq!(pause.required_queue_epoch, Some(11));
        assert_eq!(pause.media_id, expected);
        if outcome == "play" {
            submit(&mut provider, RemoteControl::Play);
        }
        acknowledge(&mut provider, &pause, true);
        expect_no_write(&mut provider);
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.sequence += 1;
        observe(&mut provider, &clock);
        let deadline = Instant::now() + Duration::from_secs(2);
        while writes().is_empty() {
            assert!(Instant::now() < deadline, "{leading}/{outcome}");
            thread::sleep(Duration::from_millis(5));
        }
        let target = format!("/v1/me/player/{outcome}?device_id=fixture-device");
        assert_eq!(writes(), [target]);
    }
}

#[test]
fn cued_previous_repairs_only_the_exact_first_paused_successor_of_a_moving_request() {
    for outcome in [
        "ack first",
        "clock first",
        "seek first",
        "pause after play dispatch",
        "repair rejected",
        "explicit pause before target",
        "explicit pause after target",
        "origin paused",
        "successor played first",
        "successor played before ack",
        "incomplete successor played before ack",
        "backoff seek first",
        "rate limited",
        "backoff explicit pause",
        "backoff explicit pause playing",
        "backoff before native dispatch",
        "backoff after native dispatch",
        "backoff ambiguous queue",
        "backoff scope lost",
        "backoff expired",
        "wrong context",
        "wrong item",
        "same epoch",
        "nonzero position",
        "ambiguous history",
        "missing play action",
        "no local request",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let preceding_seek = matches!(
            outcome,
            "seek first" | "backoff seek first" | "backoff before native dispatch"
        );
        let explicit_pause = matches!(
            outcome,
            "explicit pause before target"
                | "backoff explicit pause"
                | "backoff explicit pause playing"
                | "backoff before native dispatch"
                | "backoff after native dispatch"
        );
        let starts_in_backoff = outcome.starts_with("backoff")
            && !matches!(
                outcome,
                "backoff before native dispatch" | "backoff after native dispatch"
            );
        let set_backoff = |provider: &SpotifyRemoteProvider| {
            let now = Instant::now();
            let mut mailbox = provider.shared.0.lock().unwrap();
            let issue = PlayerError::new(ErrorCode::NetworkError, "Fixture Web API recovery");
            mailbox.control_backoff = Some(Cooldown::new(now, 5));
            mailbox.control_backoff_issue = Some(issue.clone());
            mailbox.cancel_controls(&issue, now);
        };
        let mut clock = controllable_clock(1);
        let expected = mapping::track(&track('2')).unwrap().id;
        clock.queue_epoch = Some(10);
        clock.position_ms = if preceding_seek { 6_500 } else { 3_000 };
        clock.actions.as_mut().unwrap().previous = true;
        if outcome == "origin paused" {
            clock.state = PlaybackState::Paused;
            clock.rate = 0.0;
        }
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Fixture track".into(),
            artists: vec!["Fixture artist".into()],
            album: None,
            duration_ms: 8_000,
            artwork: None,
            context_uri: Some(format!("spotify:album:{}", "a".repeat(22))),
        });
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let command = |provider: &mut SpotifyRemoteProvider| {
            let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
                matches!(event, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            command
        };
        let acknowledge =
            |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand, accepted| {
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
        let pause = |provider: &mut SpotifyRemoteProvider| {
            provider
                .submit(RemoteAction::Control {
                    control: RemoteControl::Pause,
                })
                .unwrap();
        };
        let no_native_play = |provider: &mut SpotifyRemoteProvider| {
            for _ in 0..8 {
                assert!(
                    !matches!(
                        provider.poll(),
                        Some(
                            RemoteUpdate::OutputCommand(RemoteOutputCommand {
                                control: RemoteOutputControl::Play,
                                ..
                            }) | RemoteUpdate::OutputCommandFailed {
                                control: RemoteOutputControl::Play,
                                ..
                            }
                        )
                    ),
                    "{outcome}"
                );
            }
        };
        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        let entry = RemoteOutputQueueEntry {
            media_id: expected.clone(),
            source: Some(QueueEntrySource::Context),
            metadata: None,
        };
        let previous = if matches!(outcome, "ambiguous history" | "backoff ambiguous queue") {
            vec![entry.clone(), entry]
        } else {
            vec![entry]
        };
        provider
            .submit(RemoteAction::OutputQueue {
                queue: RemoteOutputQueue {
                    generation: clock.generation,
                    epoch: 10,
                    sequence: 1,
                    device_id: clock.device_id.clone(),
                    media_id: clock.media_id.clone().unwrap(),
                    previous,
                    upcoming: vec![],
                    autoplay_ready: false,
                },
            })
            .unwrap();

        if starts_in_backoff {
            set_backoff(&provider);
        } else if outcome == "rate limited" {
            provider
                .shared
                .0
                .lock()
                .unwrap()
                .limit_requests(5, Instant::now());
        }
        if preceding_seek {
            // The first Previous after four seconds restarts the playing item.
            provider.submit_seek_with_request_id(0, 71).unwrap();
        }
        if outcome == "backoff ambiguous queue" {
            assert_eq!(
                provider
                    .submit_previous_with_request_id(72)
                    .unwrap_err()
                    .code,
                ErrorCode::NetworkError
            );
            continue;
        } else if outcome == "no local request" {
            provider
                .submit(RemoteAction::Control {
                    control: RemoteControl::Previous,
                })
                .unwrap();
        } else {
            provider.submit_previous_with_request_id(72).unwrap();
        }
        if outcome == "backoff before native dispatch" {
            pause(&mut provider);
            set_backoff(&provider);
        }
        if preceding_seek {
            let seek = command(&mut provider);
            assert_eq!(seek.control, RemoteOutputControl::Seek { position_ms: 0 });
            acknowledge(&mut provider, &seek, true);
            clock.sequence += 1;
            clock.position_ms = 0;
            observe(&mut provider, &clock);
        }
        if matches!(outcome, "backoff scope lost" | "backoff expired") {
            if outcome == "backoff scope lost" {
                clock.sequence += 1;
                clock.queue_epoch = Some(11);
                clock.media_id = Some(mapping::track(&track('3')).unwrap().id);
                observe(&mut provider, &clock);
            } else {
                thread::sleep(Duration::from_millis(470));
            }
            provider.submit(RemoteAction::Refresh).unwrap();
            assert!(!provider.owned_handoff_ready(), "{outcome}");
            for _ in 0..20 {
                assert!(
                    !matches!(provider.poll(), Some(RemoteUpdate::OutputCommand(_))),
                    "{outcome}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                server
                    .records()
                    .iter()
                    .all(|request| request.method == "GET"),
                "{outcome}"
            );
            continue;
        }
        let previous = command(&mut provider);
        assert_eq!(
            previous.control,
            RemoteOutputControl::PreviousFromStart,
            "{outcome}"
        );
        assert_eq!(
            previous.expected_previous_media_id.as_ref(),
            Some(&expected)
        );
        assert_eq!(previous.required_queue_epoch, Some(10));
        if explicit_pause && outcome != "backoff before native dispatch" {
            pause(&mut provider);
        }
        if outcome == "backoff after native dispatch" {
            set_backoff(&provider);
        }
        let delayed_receipt = matches!(
            outcome,
            "clock first"
                | "successor played before ack"
                | "incomplete successor played before ack"
        );
        if !delayed_receipt {
            acknowledge(&mut provider, &previous, true);
            assert!(!provider.owned_handoff_ready());
            no_native_play(&mut provider);
        }

        clock.sequence += 1;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        if outcome == "backoff explicit pause playing" {
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
        } else if explicit_pause {
            clock.actions.as_mut().unwrap().pause = false;
        }
        clock.position_ms = if outcome == "nonzero position" {
            121
        } else {
            0
        };
        clock.queue_epoch = Some(if outcome == "same epoch" { 10 } else { 11 });
        clock.actions.as_mut().unwrap().play = outcome != "missing play action";
        clock.media_id = Some(if outcome == "wrong item" {
            mapping::track(&track('3')).unwrap().id
        } else {
            expected.clone()
        });
        if outcome == "wrong context" {
            clock.metadata.as_mut().unwrap().context_uri =
                Some(format!("spotify:album:{}", "b".repeat(22)));
        }
        if matches!(
            outcome,
            "successor played first"
                | "successor played before ack"
                | "incomplete successor played before ack"
        ) {
            let metadata = clock.metadata.clone();
            if outcome == "incomplete successor played before ack" {
                clock.metadata.as_mut().unwrap().context_uri = None;
            }
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            observe(&mut provider, &clock);
            clock.sequence += 1;
            clock.state = PlaybackState::Paused;
            clock.rate = 0.0;
            clock.metadata = metadata;
        }
        observe(&mut provider, &clock);
        if delayed_receipt {
            assert!(!provider.owned_handoff_ready());
            no_native_play(&mut provider);
            acknowledge(&mut provider, &previous, true);
        } else if outcome == "explicit pause after target" {
            assert!(provider.owned_handoff_ready());
            pause(&mut provider);
        }

        if matches!(
            outcome,
            "ack first"
                | "clock first"
                | "seek first"
                | "backoff seek first"
                | "rate limited"
                | "pause after play dispatch"
                | "repair rejected"
        ) {
            assert!(provider.owned_handoff_ready(), "{outcome}");
            let play = command(&mut provider);
            assert_eq!(play.control, RemoteOutputControl::Play, "{outcome}");
            assert_eq!(play.request_id, Some(72));
            assert_eq!(play.required_queue_epoch, Some(11));
            assert_eq!(play.generation, clock.generation);
            assert_eq!(play.device_id, clock.device_id);
            assert_eq!(play.media_id, expected);
            assert_eq!(play.sequence, clock.sequence);
            assert!(!provider.owned_handoff_ready());
            if outcome == "pause after play dispatch" {
                pause(&mut provider);
            }
            acknowledge(&mut provider, &play, outcome != "repair rejected");
            if outcome == "pause after play dispatch" {
                // The Paused sample that authorized Play cannot satisfy a later
                // Pause after the write has been accepted.
                assert!(!provider.owned_handoff_ready());
                for _ in 0..6 {
                    assert!(!matches!(
                        provider.poll(),
                        Some(RemoteUpdate::OutputCommand(_))
                    ));
                }
                clock.sequence += 1;
                clock.state = PlaybackState::Playing;
                clock.rate = 1.0;
                observe(&mut provider, &clock);
                let trailing_pause = command(&mut provider);
                assert_eq!(trailing_pause.control, RemoteOutputControl::Pause);
                assert_eq!(trailing_pause.media_id, expected);
                acknowledge(&mut provider, &trailing_pause, true);
            }
            clock.sequence += 1;
            observe(&mut provider, &clock);
        } else if explicit_pause {
            assert!(provider.owned_handoff_ready(), "{outcome}");
            if outcome == "backoff explicit pause playing" {
                let pause = command(&mut provider);
                assert_eq!(pause.control, RemoteOutputControl::Pause);
                assert_eq!(pause.required_queue_epoch, Some(11));
                assert_eq!(pause.media_id, expected);
                acknowledge(&mut provider, &pause, true);
                clock.sequence += 1;
                clock.state = PlaybackState::Paused;
                clock.rate = 0.0;
                clock.actions.as_mut().unwrap().pause = false;
                observe(&mut provider, &clock);
            } else {
                for _ in 0..8 {
                    assert!(
                        !matches!(provider.poll(), Some(RemoteUpdate::OutputCommand(_))),
                        "{outcome}"
                    );
                }
                assert!(
                    provider.shared.0.lock().unwrap().controls.is_empty(),
                    "{outcome}"
                );
            }
        } else {
            assert!(!provider.owned_handoff_ready(), "{outcome}");
        }
        no_native_play(&mut provider);
        assert!(
            !server
                .records()
                .iter()
                .any(|request| { request.method != "GET" }),
            "{outcome}"
        );
    }
}

#[test]
fn native_previous_next_pause_uses_only_the_exact_new_context_occurrences() {
    for outcome in [
        "ack first",
        "clock first",
        "buffering",
        "repeat history",
        "repeat upcoming",
        "missing head",
        "same head",
        "stale sibling head",
        "relinked head",
        "corrected head",
        "wrong item",
        "wrong context",
        "stable same item",
        "rejected",
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        let original = clock.media_id.clone().unwrap();
        let previous = mapping::track(&track('2')).unwrap().id;
        let context = format!("spotify:album:{}", "a".repeat(22));
        clock.queue_epoch = Some(10);
        clock.options = Some(RemoteOutputOptions {
            shuffle: false,
            repeat: RepeatMode::All,
            enhancement: None,
        });
        clock.actions.as_mut().unwrap().previous = true;
        if outcome == "buffering" {
            clock.state = PlaybackState::Paused;
            clock.rate = 0.0;
            clock.actions.as_mut().unwrap().pause = false;
        }
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Fixture track".into(),
            artists: vec!["Fixture artist".into()],
            album: None,
            duration_ms: 8000,
            artwork: None,
            context_uri: Some(context),
        });
        let observe = |provider: &mut SpotifyRemoteProvider, clock: &RemoteOutputClock| {
            provider
                .submit(RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                })
                .unwrap();
        };
        let queue = |provider: &mut SpotifyRemoteProvider,
                     clock: &RemoteOutputClock,
                     previous,
                     upcoming| {
            let rows = |id: Option<MediaId>| -> Vec<RemoteOutputQueueEntry> {
                id.into_iter()
                    .map(|media_id| RemoteOutputQueueEntry {
                        media_id,
                        source: Some(QueueEntrySource::Context),
                        metadata: clock.metadata.clone(),
                    })
                    .collect()
            };
            let mut previous = rows(previous);
            if outcome == "repeat history" && clock.queue_epoch == Some(10) {
                previous.extend(previous.first().cloned());
            }
            provider
                .submit(RemoteAction::OutputQueue {
                    queue: RemoteOutputQueue {
                        generation: clock.generation,
                        epoch: clock.queue_epoch.unwrap(),
                        sequence: clock.sequence,
                        device_id: clock.device_id.clone(),
                        media_id: clock.media_id.clone().unwrap(),
                        previous,
                        upcoming: rows(upcoming),
                        autoplay_ready: false,
                    },
                })
                .unwrap();
        };
        let command = |provider: &mut SpotifyRemoteProvider| {
            let RemoteUpdate::OutputCommand(command) = wait(provider, |event| {
                matches!(event, RemoteUpdate::OutputCommand(_))
            }) else {
                unreachable!()
            };
            command
        };
        let acknowledge =
            |provider: &mut SpotifyRemoteProvider, command: &RemoteOutputCommand, id, accepted| {
                provider
                    .submit(RemoteAction::OutputCommandResult {
                        result: RemoteOutputCommandResult {
                            id,
                            generation: command.generation,
                            accepted,
                        },
                    })
                    .unwrap();
            };
        let writes = || {
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>()
        };
        let check_no_command = |provider: &mut SpotifyRemoteProvider| {
            while let Some(event) = provider.poll() {
                assert!(
                    !matches!(event, RemoteUpdate::OutputCommand(_)),
                    "{outcome}"
                );
            }
            assert!(writes().is_empty(), "{outcome}");
        };
        observe(&mut provider, &clock);
        wait(
            &mut provider,
            |event| matches!(event, RemoteUpdate::State(state) if state.owned_output_control),
        );
        queue(
            &mut provider,
            &clock,
            Some(previous.clone()),
            (outcome == "repeat upcoming").then(|| previous.clone()),
        );
        if outcome == "ack first" {
            let reads = || {
                server
                    .records()
                    .iter()
                    .filter(|request| request.target.starts_with("/v1/me/player?"))
                    .count()
            };
            let before = reads();
            server.hold_playback.store(true, Ordering::SeqCst);
            provider.submit(RemoteAction::Refresh).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while reads() == before {
                assert!(
                    Instant::now() < deadline,
                    "The Web playback read did not start"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
        provider.submit_previous_with_request_id(72).unwrap();
        let prev = command(&mut provider);
        assert_eq!(prev.control, RemoteOutputControl::PreviousFromStart);
        assert_eq!(prev.request_id, Some(72));
        assert_eq!(prev.required_queue_epoch, Some(10));
        assert_eq!(prev.media_id, original);
        assert_eq!(
            prev.expected_previous_media_id.as_ref(),
            Some(&previous),
            "{outcome}"
        );
        assert_eq!(
            serde_json::to_value(&prev)
                .unwrap()
                .get("expected_previous_media_id")
                .and_then(serde_json::Value::as_str),
            Some(previous.as_str()),
            "{outcome}"
        );
        provider.submit_next_with_request_id(73).unwrap();
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Pause,
            })
            .unwrap();
        assert!(!provider.owned_handoff_ready());
        if outcome != "buffering" {
            acknowledge(&mut provider, &prev, prev.id, true);
        }
        clock.media_id = Some(previous.clone());
        clock.queue_epoch = Some(11);
        clock.sequence += 1;
        clock.position_ms = 0;
        let mut actions = clock.actions.take();
        if outcome == "buffering" {
            clock.state = PlaybackState::Loading;
            actions.as_mut().unwrap().pause = true;
        }
        observe(&mut provider, &clock);
        assert!(!provider.owned_handoff_ready());
        check_no_command(&mut provider);
        if outcome == "ack first" {
            queue(&mut provider, &clock, None, Some(original.clone()));
            assert!(!provider.owned_handoff_ready());
        }
        clock.sequence += 1;
        clock.actions = actions;
        observe(&mut provider, &clock);
        if outcome != "ack first" {
            assert!(!provider.owned_handoff_ready());
            queue(
                &mut provider,
                &clock,
                None,
                match outcome {
                    "missing head" => None,
                    "same head" => Some(previous.clone()),
                    // A queue for the outgoing item can arrive late with the
                    // predecessor's new epoch assigned by the host.
                    "stale sibling head" | "corrected head" | "buffering" => {
                        Some(mapping::track(&track('3')).unwrap().id)
                    }
                    "relinked head" => {
                        let mut relinked = track('4');
                        relinked["linked_from"] = json!({ "uri": original.as_str() });
                        Some(mapping::track(&relinked).unwrap().id)
                    }
                    _ => Some(original.clone()),
                },
            );
        }
        if outcome == "buffering" {
            // A complete Buffering clock and a queue still cannot release a
            // follower before Previous ACK or with an unrelated queue head.
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            acknowledge(&mut provider, &prev, prev.id, true);
        }
        if matches!(outcome, "corrected head" | "buffering") {
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            clock.sequence += 1;
            observe(&mut provider, &clock);
            queue(&mut provider, &clock, None, Some(original.clone()));
        }
        if matches!(
            outcome,
            "missing head" | "same head" | "stale sibling head" | "relinked head"
        ) {
            assert!(!provider.owned_handoff_ready());
            let deadline = Instant::now() + Duration::from_secs(2);
            while writes().is_empty() {
                assert!(Instant::now() < deadline, "{outcome}");
                assert!(!matches!(
                    provider.poll(),
                    Some(RemoteUpdate::OutputCommand(_))
                ));
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(writes(), ["/v1/me/player/next?device_id=fixture-device"]);
            continue;
        }
        assert!(provider.owned_handoff_ready(), "{outcome}");
        let next = command(&mut provider);
        assert_eq!(next.control, RemoteOutputControl::Next);
        assert_eq!(next.request_id, Some(73));
        assert_eq!(next.required_queue_epoch, Some(11));
        assert_eq!(next.media_id, previous);
        assert!(
            serde_json::to_value(&next)
                .unwrap()
                .get("expected_previous_media_id")
                .is_none()
        );
        if outcome == "rejected" {
            acknowledge(&mut provider, &next, next.id, false);
            check_no_command(&mut provider);
            continue;
        }
        if outcome != "clock first" {
            acknowledge(&mut provider, &next, next.id, true);
        }
        assert!(!provider.owned_handoff_ready());
        check_no_command(&mut provider);
        let successor_epoch = if outcome == "ack first" {
            // Soloist's incomplete relative boundary publishes actionless
            // Loading with the outgoing URI and an invalidated queue epoch.
            let actions = clock.actions.take();
            let metadata = clock.metadata.take();
            clock.state = PlaybackState::Loading;
            clock.rate = 0.0;
            clock.queue_epoch = Some(12);
            clock.sequence += 1;
            observe(&mut provider, &clock);
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            clock.actions = actions;
            clock.metadata = metadata;
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            13
        } else {
            12
        };
        clock.media_id = Some(match outcome {
            "wrong item" => mapping::track(&track('3')).unwrap().id,
            "stable same item" => previous.clone(),
            _ => original.clone(),
        });
        clock.queue_epoch = Some(successor_epoch);
        clock.sequence += 1;
        if outcome == "wrong context" {
            clock.metadata.as_mut().unwrap().context_uri =
                Some(format!("spotify:album:{}", "b".repeat(22)));
        }
        if outcome == "ack first" {
            let actions = clock.actions.take();
            observe(&mut provider, &clock);
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            clock.actions = actions;
            clock.sequence += 1;
        }
        observe(&mut provider, &clock);
        if outcome == "buffering" {
            // Next already left the predecessor. Preserve the normal Pause
            // handoff while the actual Next target is still Buffering.
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            clock.sequence += 1;
            observe(&mut provider, &clock);
        }
        if matches!(outcome, "wrong item" | "wrong context" | "stable same item") {
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            continue;
        }
        if outcome == "clock first" {
            acknowledge(&mut provider, &next, next.id + 1, true);
            assert!(!provider.owned_handoff_ready());
            check_no_command(&mut provider);
            acknowledge(&mut provider, &next, next.id, true);
        }
        assert!(provider.owned_handoff_ready());
        let pause = command(&mut provider);
        assert_eq!(pause.control, RemoteOutputControl::Pause);
        assert_eq!(pause.request_id, None);
        assert_eq!(pause.required_queue_epoch, Some(successor_epoch));
        assert_eq!(pause.media_id, original);
        acknowledge(&mut provider, &pause, pause.id, true);
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.sequence += 1;
        observe(&mut provider, &clock);
        check_no_command(&mut provider);
        server.hold_playback.store(false, Ordering::SeqCst);
    }
}

#[test]
fn successor_pause_requires_a_still_fresh_predecessor_until_previous_is_acknowledged() {
    let now = Instant::now();
    let context = format!("spotify:album:{}", "a".repeat(22));
    let mut clock = controllable_clock(1);
    clock.queue_epoch = Some(10);
    clock.metadata = Some(RemoteOutputMetadata {
        title: "Fixture track".into(),
        artists: vec!["Fixture artist".into()],
        album: None,
        duration_ms: 8000,
        artwork: None,
        context_uri: Some(context.clone()),
    });
    let mut lane = owned_http::Lane::new(1, &clock, now, None, None, now).unwrap();
    let predecessor = mapping::track(&track('2')).unwrap().id;
    lane.pin_previous(
        &clock,
        Some(owned_http::PreviousWitness {
            predecessor: predecessor.clone(),
            context,
            queue_epoch: 10,
            observed_at: now - Duration::from_millis(850),
            unambiguous_context_predecessor: true,
        }),
    );
    lane.begin_relative(now);
    clock.media_id = Some(predecessor);
    clock.sequence += 1;
    clock.queue_epoch = Some(11);
    assert!(lane.observe(Some((&clock, now + Duration::from_millis(30)))));
    assert!(lane.native_pause_candidate(&clock, now + Duration::from_millis(50)));
    assert!(!lane.native_pause_candidate(&clock, now + Duration::from_millis(51)));
    lane.accepted(now + Duration::from_millis(60));
    assert!(lane.native_pause_candidate(&clock, now + Duration::from_millis(61)));
}

#[test]
fn timed_out_previous_reconciles_only_the_expected_local_effect() {
    enum Outcome {
        ExpectedSuccessor,
        DifferentContext,
        Transfer { with_predecessor: bool },
        Volume,
    }

    fn scenario(outcome: Outcome) {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let mut clock = controllable_clock(1);
        clock.queue_epoch = Some(1);
        clock.actions.as_mut().unwrap().previous = true;
        let item = mapping::track(&track('2')).unwrap();
        let metadata = |album: &str| RemoteOutputMetadata {
            title: item.title.clone(),
            artists: item.artists.clone(),
            album: item.album.clone(),
            duration_ms: item.duration_ms,
            artwork: item.artwork.clone(),
            context_uri: Some(format!("spotify:album:{album}")),
        };
        clock.metadata = Some(metadata(&"a".repeat(22)));
        provider
            .submit(RemoteAction::OutputClock {
                clock: Some(clock.clone()),
            })
            .unwrap();
        wait(
            &mut provider,
            |u| matches!(u, RemoteUpdate::State(s) if s.owned_output_control),
        );
        if !matches!(
            outcome,
            Outcome::Transfer {
                with_predecessor: false
            }
        ) {
            provider
                .submit(RemoteAction::OutputQueue {
                    queue: RemoteOutputQueue {
                        generation: clock.generation,
                        device_id: clock.device_id.clone(),
                        media_id: clock.media_id.clone().unwrap(),
                        epoch: 1,
                        sequence: 1,
                        autoplay_ready: false,
                        upcoming: vec![],
                        previous: vec![RemoteOutputQueueEntry {
                            media_id: item.id.clone(),
                            source: Some(QueueEntrySource::Context),
                            metadata: None,
                        }],
                    },
                })
                .unwrap();
        }
        server.hold_previous_response.store(true, Ordering::SeqCst);
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Previous,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server
            .records()
            .iter()
            .any(|request| request.method == "POST")
        {
            assert!(Instant::now() < deadline, "Previous was not sent");
            thread::sleep(Duration::from_millis(5));
        }
        provider
            .submit(RemoteAction::Control {
                control: RemoteControl::Pause,
            })
            .unwrap();
        if matches!(outcome, Outcome::Transfer { .. }) {
            provider
                .submit(RemoteAction::Control {
                    control: RemoteControl::Transfer {
                        device_id: "fixture-device".into(),
                        operation_id: Some(77),
                    },
                })
                .unwrap();
            // Keep the first response delayed beyond the actual client deadline.
            thread::sleep(Duration::from_millis(5050));
            server.hold_previous_response.store(false, Ordering::SeqCst);
            wait(
                &mut provider,
                |u| matches!(u, RemoteUpdate::State(s) if s.control_result.as_ref().is_some_and(|result| result.operation_id == 77 && result.error.is_none())),
            );
            assert_eq!(
                server
                    .records()
                    .into_iter()
                    .filter(|request| request.method != "GET")
                    .map(|request| request.target)
                    .collect::<Vec<_>>(),
                [
                    "/v1/me/player/previous?device_id=fixture-device",
                    "/v1/me/player?"
                ]
            );
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while !provider
            .shared
            .0
            .lock()
            .unwrap()
            .owned_http
            .as_ref()
            .is_some_and(owned_http::Lane::observing_previous)
        {
            assert!(
                Instant::now() < deadline,
                "Expected response timeout to enter observation grace"
            );
            thread::sleep(Duration::from_millis(5));
        }
        if matches!(outcome, Outcome::Volume) {
            assert!(
                provider
                    .submit(RemoteAction::Control {
                        control: RemoteControl::Volume { volume: 0.4 },
                    })
                    .is_err(),
                "After the response timeout, an expired owned clock cannot grant a new HTTP setting"
            );
            assert!(
                provider.shared.0.lock().unwrap().owned_http.is_some(),
                "Rejected stale Volume must preserve Previous's observation grace"
            );
            server.hold_previous_response.store(false, Ordering::SeqCst);
            wait(
                &mut provider,
                |u| matches!(u, RemoteUpdate::State(s) if s.issue.as_ref().is_some_and(|error| error.message == "Spotify did not confirm the previous-track command.")),
            );
            assert!(provider.shared.0.lock().unwrap().owned_http.is_none());
            assert_eq!(
                server
                    .records()
                    .into_iter()
                    .filter(|request| request.method != "GET")
                    .map(|request| request.target)
                    .collect::<Vec<_>>(),
                ["/v1/me/player/previous?device_id=fixture-device"]
            );
            return;
        }
        server.hold_previous_response.store(false, Ordering::SeqCst);
        let exact_context = matches!(outcome, Outcome::ExpectedSuccessor);
        clock.sequence = 2;
        clock.queue_epoch = Some(2);
        clock.media_id = Some(item.id.clone());
        clock.metadata = Some(metadata(&if exact_context { "a" } else { "b" }.repeat(22)));
        provider
            .submit(RemoteAction::OutputClock { clock: Some(clock) })
            .unwrap();
        if exact_context {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let active = provider.shared.0.lock().unwrap().owned_http.is_some();
                while let Some(update) = provider.poll() {
                    assert!(
                        !matches!(update, RemoteUpdate::State(s) if s.issue.as_ref().is_some_and(|error| error.code == ErrorCode::NetworkError))
                    );
                }
                if !active {
                    break;
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        } else {
            wait(
                &mut provider,
                |u| matches!(u, RemoteUpdate::State(s) if s.issue.as_ref().is_some_and(|error| error.message == "Spotify did not confirm the previous-track command.")),
            );
        }
        assert_eq!(
            server
                .records()
                .into_iter()
                .filter(|request| request.method != "GET")
                .map(|request| request.target)
                .collect::<Vec<_>>(),
            ["/v1/me/player/previous?device_id=fixture-device"]
        );
    }
    thread::scope(|scope| {
        scope.spawn(|| scenario(Outcome::ExpectedSuccessor));
        scope.spawn(|| scenario(Outcome::DifferentContext));
        scope.spawn(|| {
            scenario(Outcome::Transfer {
                with_predecessor: true,
            })
        });
        scope.spawn(|| {
            scenario(Outcome::Transfer {
                with_predecessor: false,
            })
        });
        scope.spawn(|| scenario(Outcome::Volume));
    });
}
