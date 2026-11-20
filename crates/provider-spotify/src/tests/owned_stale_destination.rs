use super::*;

fn verified_output(at: Instant) -> output_clock::OutputProjection {
    let mut output = output_clock::OutputProjection::default();
    output.ingest(output_base(1), at);
    output.take(at);
    let mut clock = output_clock(1);
    clock.control_uptime_ms = Some(1_000);
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        next: true,
        previous: true,
        seek: true,
        shuffle: true,
        repeat: true,
        ..Default::default()
    });
    output.observe(Some(clock), at).unwrap();
    assert!(output.take(at).unwrap().owned_output_control);
    assert!(output.has_owned_destination_veto());
    output
}

#[test]
fn stale_owned_destination_veto_survives_poll_nil_expiry_and_inactive_new_runtime() {
    for boundary in ["polled stale", "nil", "expired", "inactive runtime"] {
        let now = Instant::now();
        let start = now - Duration::from_secs(if boundary == "expired" { 8 } else { 2 });
        let mut output = verified_output(start);
        match boundary {
            "nil" => {
                output.observe(None, now).unwrap();
            }
            "inactive runtime" => {
                let mut clock = output_clock(1);
                clock.generation = 2;
                clock.active = false;
                clock.state = PlaybackState::Idle;
                clock.media_id = None;
                clock.rate = 0.0;
                clock.control_uptime_ms = Some(3_000);
                output.observe(Some(clock), now).unwrap();
            }
            _ => {}
        }
        let state = output.take(now).unwrap();
        assert!(output.has_owned_destination_veto(), "{boundary}");
        assert!(output.control_clock(now).is_none(), "{boundary}");
        assert!(output.navigation_clock(now).is_none(), "{boundary}");
        assert!(
            !state.owned_output_control,
            "the veto grants no rights: {boundary}"
        );
        assert!(output.owned_capture_witness(now).is_none());
        if boundary == "expired" || boundary == "nil" {
            assert!(!state.owned_output_observed);
            assert_eq!(
                state.playback.unwrap().position_ms,
                1234,
                "the veto cannot construct selected metadata"
            );
        }
    }
}

#[test]
fn stale_owned_destination_veto_requires_a_new_authoritative_external_playback_read() {
    for scenario in [
        "fresh external",
        "same raw read",
        "devices only",
        "204",
        "missing item",
        "inactive",
        "restricted",
        "contradictory row",
        "unavailable",
    ] {
        let now = Instant::now();
        let mut output = verified_output(now - Duration::from_secs(8));
        output.take(now);
        let mut state = output_base(
            if scenario == "same raw read" || scenario == "devices only" {
                1
            } else {
                2
            },
        );
        let mut external = state
            .playback
            .as_ref()
            .unwrap()
            .device
            .as_ref()
            .unwrap()
            .clone();
        external.id = "external-device".into();
        external.name = "Other speaker".into();
        state.playback.as_mut().unwrap().device = Some(external.clone());
        match scenario {
            "devices only" => {
                state.playback = output_base(1).playback;
                state.devices = vec![external];
            }
            "204" => {
                state.playback = None;
                state.devices = vec![external];
            }
            "missing item" => {
                state.playback.as_mut().unwrap().current = None;
            }
            "inactive" => {
                state
                    .playback
                    .as_mut()
                    .unwrap()
                    .device
                    .as_mut()
                    .unwrap()
                    .is_active = false;
            }
            "restricted" => {
                state
                    .playback
                    .as_mut()
                    .unwrap()
                    .device
                    .as_mut()
                    .unwrap()
                    .is_restricted = true;
            }
            "contradictory row" => {
                state
                    .devices
                    .push(output_base(1).playback.unwrap().device.unwrap());
            }
            "unavailable" => {
                state.connection = ProviderConnectionState::Unavailable;
            }
            _ => {}
        }
        output.ingest(state, now);
        assert_eq!(
            output.has_owned_destination_veto(),
            scenario != "fresh external",
            "{scenario}"
        );
        let state = output.take(now).unwrap();
        assert!(!state.owned_output_control && !state.owned_output_observed);
    }
    for auth in [false, true] {
        let now = Instant::now();
        let mut output = verified_output(now);
        if auth {
            output.ingest(disconnected(true), now);
        } else {
            output.reset();
        }
        assert!(
            !output.has_owned_destination_veto(),
            "account reset drops inert evidence"
        );
    }
}

#[test]
fn stale_owned_destination_veto_rejects_generic_controls_without_any_http_or_native_dispatch() {
    for boundary in ["polled stale", "nil", "expired", "inactive runtime"] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let now = Instant::now();
        provider.output =
            verified_output(now - Duration::from_secs(if boundary == "expired" { 8 } else { 2 }));
        if boundary == "nil" {
            provider.output.observe(None, now).unwrap();
        }
        if boundary == "inactive runtime" {
            let mut clock = output_clock(1);
            clock.generation = 2;
            clock.active = false;
            clock.state = PlaybackState::Idle;
            clock.media_id = None;
            clock.rate = 0.0;
            provider.output.observe(Some(clock), now).unwrap();
        }
        provider.output.take(now);
        let item = mapping::track(&track('4')).unwrap();
        for control in [
            RemoteControl::Play,
            RemoteControl::Pause,
            RemoteControl::Next,
            RemoteControl::Previous,
            RemoteControl::Seek { position_ms: 2_000 },
            RemoteControl::Volume { volume: 0.3 },
            RemoteControl::Shuffle { enabled: true },
            RemoteControl::Repeat {
                mode: RepeatMode::All,
            },
            RemoteControl::Enqueue { item },
        ] {
            assert!(
                provider.submit(RemoteAction::Control { control }).is_err(),
                "{boundary}"
            );
        }
        while let Some(update) = provider.poll() {
            assert!(
                !matches!(update, RemoteUpdate::OutputCommand(_)),
                "a veto creates no native dispatch"
            );
        }
        assert!(
            server.records().iter().all(|r| r.method == "GET"),
            "no HTTP mutation: {boundary}"
        );
        assert!(provider.shared.0.lock().unwrap().controls.is_empty());
    }
}

#[test]
fn fresh_external_takeover_releases_stale_owned_destination_veto_for_normal_http() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    let now = Instant::now();
    provider.output = verified_output(now - Duration::from_secs(8));
    provider.output.take(now);
    let mut external = output_base(2);
    let device = external.playback.as_mut().unwrap().device.as_mut().unwrap();
    device.id = "external-device".into();
    device.name = "Other speaker".into();
    provider.output.ingest(external, now);
    provider.output.take(now);
    assert!(!provider.output.has_owned_destination_veto());
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !server
        .records()
        .iter()
        .any(|r| r.method == "POST" && r.target.starts_with("/v1/me/player/next"))
    {
        assert!(
            Instant::now() < deadline,
            "external HTTP navigation was blocked"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn stale_owned_destination_veto_keeps_explicit_addressed_control_paths() {
    let item = mapping::track(&track('4')).unwrap();
    for control in [
        RemoteControl::Start { item: item.clone() },
        RemoteControl::StartOnDevice {
            item: item.clone(),
            device_id: "fixture-device".into(),
            operation_id: Some(11),
        },
        RemoteControl::StartContext {
            uri: format!("spotify:album:{}", "a".repeat(22)),
            device_id: "fixture-device".into(),
            operation_id: Some(12),
            item: None,
            position: None,
            query_id: None,
        },
        RemoteControl::ResumeOnDevice {
            item: item.clone(),
            device_id: "fixture-device".into(),
            position_ms: 1_000,
            context_uri: None,
            operation_id: Some(13),
        },
        RemoteControl::Transfer {
            device_id: "external-device".into(),
            operation_id: Some(14),
        },
        RemoteControl::PauseOnDevice {
            device_id: "fixture-device".into(),
        },
    ] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let now = Instant::now();
        provider.output = verified_output(now - Duration::from_secs(8));
        provider.output.take(now);
        assert!(provider.output.has_owned_destination_veto());
        provider.submit(RemoteAction::Control { control }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.records().iter().any(|r| r.method != "GET") {
            assert!(
                Instant::now() < deadline,
                "explicit addressed control was blocked"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn unplayed_cloud_selection_cannot_seed_a_stale_owned_destination_veto() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    output.ingest(output_base(1), now);
    output.take(now);
    output.observe(None, now).unwrap();
    output.take(now);
    assert!(!output.has_owned_destination_veto());
}
