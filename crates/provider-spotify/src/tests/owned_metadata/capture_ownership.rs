use super::*;

fn proven_capture(
    now: Instant,
) -> (
    output_clock::OutputProjection,
    RemoteState,
    RemoteOutputClock,
    OwnedCaptureWitness,
) {
    let (mut output, mut base) =
        begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
    acknowledge_inactive_context_restart(&mut output, &mut base, now);
    observe_inactive_context_restart(
        &mut output,
        inactive_context_clock(2, PlaybackState::Loading),
        now + Duration::from_millis(100),
    );
    let playing = inactive_context_clock(3, PlaybackState::Playing);
    let frame = observe_inactive_context_restart(
        &mut output,
        playing.clone(),
        now + Duration::from_millis(200),
    );
    assert!(frame.owned_output_observed && !frame.owned_output_control);
    let proof = output
        .owned_capture_witness(now + Duration::from_millis(200))
        .expect("the exact accepted restart grants capture only");
    (output, base, playing, proof)
}

#[test]
fn capture_only_destination_veto_outlives_clock_and_never_grants_relative_controls() {
    for boundary in ["fresh capture", "expired capture", "nil"] {
        let server = Server::new();
        let mut provider = server.provider();
        connect(&mut provider);
        let now = Instant::now();
        let age = if boundary == "fresh capture" {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(8)
        };
        let (mut output, _, _, _) = proven_capture(now - age);
        if boundary == "nil" {
            output.observe(None, now).unwrap();
        }
        output.take(now);
        assert!(output.has_owned_destination_veto());
        assert!(output.control_clock(now).is_none() && !output.has_owned_binding());
        provider.output = output;
        for control in [
            RemoteControl::Next,
            RemoteControl::Previous,
            RemoteControl::Play,
            RemoteControl::Pause,
        ] {
            assert!(
                provider.submit(RemoteAction::Control { control }).is_err(),
                "{boundary}"
            );
        }
        assert!(server.records().iter().all(|r| r.method == "GET"));
    }
}

#[test]
fn accepted_restart_capture_is_available_through_immediate_pause_without_control_grant() {
    let now = Instant::now();
    let (mut output, _, mut clock, proof) = proven_capture(now);
    assert_eq!(proof.generation, clock.generation);
    assert_eq!(proof.device_id, clock.device_id);
    assert_eq!(Some(&proof.media_id), clock.media_id.as_ref());
    assert_eq!(Some(proof.queue_epoch), clock.queue_epoch);
    assert_eq!(proof.playing_sequence, clock.sequence);
    assert_eq!(
        Some(proof.playing_control_uptime_ms),
        clock.control_uptime_ms
    );
    output.remember_selection(
        &RemoteControl::PauseOnDevice {
            device_id: clock.device_id.clone(),
        },
        now + Duration::from_millis(201),
    );
    clock.sequence += 1;
    clock.state = PlaybackState::Paused;
    clock.rate = 0.0;
    clock.control_uptime_ms = Some(1_400);
    output
        .observe(Some(clock.clone()), now + Duration::from_millis(210))
        .unwrap();
    // The consumer can read the original Playing proof and the newest paused
    // clock together even if it never polled between their two packets.
    assert_eq!(
        output.owned_capture_witness(now + Duration::from_millis(210)),
        Some(proof.clone())
    );
    assert_eq!(
        output.observed_clock().unwrap().state,
        PlaybackState::Paused
    );
    let frame = output.take(now + Duration::from_millis(210)).unwrap();
    assert!(!frame.owned_output_control && !output.has_owned_binding());
    assert!(
        output
            .control_clock(now + Duration::from_millis(210))
            .is_none()
    );
    clock.sequence += 1;
    clock.actions = None;
    clock.metadata = None;
    clock.control_uptime_ms = Some(1_500);
    output
        .observe(Some(clock), now + Duration::from_millis(250))
        .unwrap();
    assert_eq!(
        output.owned_capture_witness(now + Duration::from_millis(250)),
        Some(proof)
    );
    let partial = output.take(now + Duration::from_millis(250)).unwrap();
    assert!(partial.owned_output_observed && !partial.owned_output_control);
    assert_eq!(partial.playback.unwrap().state, PlaybackState::Paused);
}

#[test]
fn capture_scope_outlives_selection_expiry_without_rebasing_its_playing_uptime() {
    let now = Instant::now();
    let (mut output, _, mut clock, proof) = proven_capture(now);
    for tick in 1..=200 {
        clock.sequence += 1;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.actions = None;
        clock.control_uptime_ms = Some(proof.playing_control_uptime_ms + tick * 250);
        let at = now + Duration::from_millis(200 + tick * 250);
        output.observe(Some(clock.clone()), at).unwrap();
        let frame = output.take(at).unwrap();
        assert!(frame.owned_output_observed);
        assert_eq!(
            frame
                .playback
                .as_ref()
                .unwrap()
                .current
                .as_ref()
                .map(|item| &item.id),
            Some(&proof.media_id)
        );
        assert!(!frame.owned_output_control && !output.has_owned_binding());
        assert!(output.control_clock(at).is_none());
        assert_eq!(output.owned_capture_witness(at), Some(proof.clone()));
    }
    assert_eq!(
        output.owned_capture_witness(now + Duration::from_secs(52)),
        None
    );
    // A new packet after a freshness gap cannot resurrect the same operation.
    clock.sequence += 1;
    clock.state = PlaybackState::Playing;
    clock.rate = 1.0;
    output
        .observe(Some(clock), now + Duration::from_secs(52))
        .unwrap();
    output.take(now + Duration::from_secs(52));
    assert!(
        output
            .owned_capture_witness(now + Duration::from_secs(52))
            .is_none()
    );
}

#[test]
fn a_promoted_capture_survives_omitted_cloud_devices_and_playback_204_only_with_fresh_clocks() {
    for paused in [false, true] {
        let now = Instant::now();
        let (mut output, mut base, mut clock, proof) = proven_capture(now);
        base.devices.clear();
        base.playback = None;
        for tick in 1..=32 {
            let at = now + Duration::from_millis(200 + tick * 250);
            clock.sequence += 1;
            clock.control_uptime_ms = Some(proof.playing_control_uptime_ms + tick * 250);
            if paused {
                clock.state = PlaybackState::Paused;
                clock.rate = 0.0;
                clock.actions = None;
                clock.metadata = None;
            }
            output.observe(Some(clock.clone()), at).unwrap();
            base.observation_id += 1;
            output.ingest(base.clone(), at);
            let projected = output.take(at).unwrap();
            assert!(
                projected.devices.is_empty(),
                "retention must not fabricate a cloud device row"
            );
            assert!(projected.owned_output_observed && !projected.owned_output_control);
            let playback = projected.playback.unwrap();
            assert_eq!(playback.current.unwrap().id, proof.media_id);
            assert_eq!(playback.device.unwrap().id, proof.device_id);
            assert_eq!(
                playback.state,
                if paused {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                }
            );
            assert!(!output.has_owned_binding() && output.control_clock(at).is_none());
            assert_eq!(output.owned_capture_witness(at), Some(proof.clone()));
        }
        let expired = now + Duration::from_secs(10);
        output.ingest(base, expired);
        assert!(output.owned_capture_witness(expired).is_none());
        clock.sequence += 1;
        clock.control_uptime_ms = Some(proof.playing_control_uptime_ms + 10_000);
        output.observe(Some(clock), expired).unwrap();
        output.take(expired);
        assert!(
            output.owned_capture_witness(expired).is_none(),
            "a later packet cannot recreate a revoked capture identity"
        );
    }
}

#[test]
fn an_existing_normal_binding_keeps_metadata_during_device_omission_and_cloud_204() {
    let now = Instant::now();
    let mut output = output_clock::OutputProjection::default();
    let mut base = output_base(1);
    base.devices.push(
        base.playback
            .as_ref()
            .unwrap()
            .device
            .as_ref()
            .unwrap()
            .clone(),
    );
    output.ingest(base.clone(), now);
    let mut clock = output_clock(1);
    clock.control_uptime_ms = Some(1_000);
    clock.queue_epoch = Some(1);
    clock.actions = Some(RemoteOutputActions {
        pause: true,
        next: true,
        ..Default::default()
    });
    output.observe(Some(clock.clone()), now).unwrap();
    assert!(output.take(now).unwrap().owned_output_control);
    base.devices.clear();
    base.playback = None;
    for tick in 1..=32 {
        let at = now + Duration::from_millis(tick * 250);
        clock.sequence += 1;
        clock.control_uptime_ms = Some(1_000 + tick * 250);
        output.observe(Some(clock.clone()), at).unwrap();
        base.observation_id += 1;
        output.ingest(base.clone(), at);
        let projected = output.take(at).unwrap();
        assert!(projected.devices.is_empty());
        assert!(projected.owned_output_observed && projected.owned_output_control);
        assert_eq!(
            projected.playback.unwrap().device.unwrap().id,
            clock.device_id
        );
    }
}

#[test]
fn capture_witness_revokes_on_foreign_scope_clock_loss_or_new_selection() {
    for invalid in [
        "runtime",
        "device",
        "media",
        "context",
        "epoch",
        "inactive",
        "idle",
        "loading",
        "uptime",
        "null",
        "reset",
        "new selection",
        "foreign pause",
        "unaddressed pause",
    ] {
        let now = Instant::now();
        let (mut output, _, mut clock, _) = proven_capture(now);
        clock.sequence += 1;
        match invalid {
            "runtime" => clock.generation += 1,
            "device" => clock.device_id = "foreign".into(),
            "media" => clock.media_id = Some(mapping::track(&track('2')).unwrap().id),
            "context" => {
                clock.metadata.as_mut().unwrap().context_uri =
                    Some(format!("spotify:album:{}", "b".repeat(22)))
            }
            "epoch" => clock.queue_epoch = clock.queue_epoch.map(|epoch| epoch + 1),
            "inactive" => clock.active = false,
            "idle" => clock.state = PlaybackState::Idle,
            "loading" => {
                clock.state = PlaybackState::Loading;
                clock.rate = 0.0;
            }
            "uptime" => clock.control_uptime_ms = Some(1),
            "null" => {
                output
                    .observe(None, now + Duration::from_millis(250))
                    .unwrap();
            }
            "reset" => output.reset(),
            "new selection" => output.remember_selection(
                &RemoteControl::StartContext {
                    uri: format!("spotify:album:{}", "b".repeat(22)),
                    device_id: clock.device_id.clone(),
                    operation_id: Some(26),
                    item: None,
                    position: None,
                    query_id: None,
                },
                now + Duration::from_millis(250),
            ),
            "foreign pause" => output.remember_selection(
                &RemoteControl::PauseOnDevice {
                    device_id: "foreign".into(),
                },
                now + Duration::from_millis(250),
            ),
            "unaddressed pause" => {
                output.remember_selection(&RemoteControl::Pause, now + Duration::from_millis(250))
            }
            _ => unreachable!(),
        }
        if !matches!(
            invalid,
            "null" | "reset" | "new selection" | "foreign pause" | "unaddressed pause"
        ) {
            output
                .observe(Some(clock), now + Duration::from_millis(250))
                .unwrap();
        }
        assert!(
            output
                .owned_capture_witness(now + Duration::from_millis(250))
                .is_none(),
            "{invalid}"
        );
    }
}

#[test]
fn capture_promotion_requires_ack_full_restart_and_known_uncontested_device() {
    for invalid in [
        "no ack",
        "no loading",
        "partial loading",
        "partial playing",
        "unknown",
        "restricted",
        "external active",
    ] {
        let now = Instant::now();
        let (mut output, mut base) =
            begin_inactive_context_restart(now, inactive_context_clock(1, PlaybackState::Paused));
        if invalid != "no ack" {
            acknowledge_inactive_context_restart(&mut output, &mut base, now);
        }
        if invalid != "no loading" {
            let mut loading = inactive_context_clock(2, PlaybackState::Loading);
            if invalid == "partial loading" {
                loading.actions = None;
            }
            observe_inactive_context_restart(
                &mut output,
                loading,
                now + Duration::from_millis(100),
            );
        }
        match invalid {
            "unknown" => base.devices.clear(),
            "restricted" => base.devices[0].is_restricted = true,
            "external active" => {
                let mut external = base.devices[0].clone();
                external.id = "external".into();
                external.is_active = true;
                base.devices.push(external);
            }
            _ => {}
        }
        output.ingest(base, now + Duration::from_millis(150));
        let mut playing = inactive_context_clock(3, PlaybackState::Playing);
        if invalid == "partial playing" {
            playing.actions = None;
        }
        observe_inactive_context_restart(&mut output, playing, now + Duration::from_millis(200));
        assert!(
            output
                .owned_capture_witness(now + Duration::from_millis(200))
                .is_none(),
            "{invalid}"
        );
    }
}

#[test]
fn capture_binding_revokes_on_auth_or_device_conflict_without_resurrection() {
    for invalid in [
        "auth",
        "disconnected",
        "renamed",
        "restricted",
        "external list",
        "external frame",
        "inactive external frame",
        "missing frame device",
        "ambiguous name",
        "replacement name",
    ] {
        let now = Instant::now();
        let (mut output, mut base, mut clock, _) = proven_capture(now);
        let original = base.clone();
        match invalid {
            "auth" => base.authentication_required = true,
            "disconnected" => base.connection = ProviderConnectionState::Disconnected,
            "renamed" => base.devices[0].name = "Renamed device".into(),
            "restricted" => base.devices[0].is_restricted = true,
            "external list" => {
                let mut external = base.devices[0].clone();
                external.id = "external".into();
                external.is_active = true;
                base.devices.push(external);
            }
            "external frame" => {
                let mut playback = output_base(2).playback.unwrap();
                let device = playback.device.as_mut().unwrap();
                device.id = "external".into();
                device.is_active = true;
                base.playback = Some(playback);
            }
            "inactive external frame" => {
                let mut playback = output_base(2).playback.unwrap();
                let device = playback.device.as_mut().unwrap();
                device.id = "external".into();
                device.is_active = false;
                base.playback = Some(playback);
            }
            "missing frame device" => {
                let mut playback = output_base(2).playback.unwrap();
                playback.device = None;
                base.playback = Some(playback);
            }
            "ambiguous name" => {
                let mut other = base.devices[0].clone();
                other.id = "other".into();
                other.is_active = false;
                base.devices.push(other);
            }
            "replacement name" => base.devices[0].id = "replacement".into(),
            _ => unreachable!(),
        }
        output.ingest(base, now + Duration::from_millis(250));
        assert!(
            output
                .owned_capture_witness(now + Duration::from_millis(250))
                .is_none(),
            "{invalid}"
        );
        output.ingest(original, now + Duration::from_millis(300));
        clock.sequence += 1;
        output
            .observe(Some(clock), now + Duration::from_millis(300))
            .unwrap();
        output.take(now + Duration::from_millis(300));
        assert!(
            output
                .owned_capture_witness(now + Duration::from_millis(300))
                .is_none(),
            "{invalid}"
        );
    }
}

#[test]
fn provider_capture_accessor_is_separate_from_native_control_and_auth_revokes_it() {
    let server = Server::new();
    let mut provider = server.provider();
    let (output, _, _, proof) = proven_capture(Instant::now());
    provider.output = output;
    assert_eq!(
        RemoteProvider::owned_capture_witness(&provider),
        Some(proof)
    );
    assert!(provider.output.control_clock(Instant::now()).is_none());
    provider.shared.0.lock().unwrap().authorization_revoked = true;
    assert!(RemoteProvider::owned_capture_witness(&provider).is_none());
    assert!(server.records().is_empty());
}
