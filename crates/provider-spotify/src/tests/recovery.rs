use super::*;

fn query(query_id: u64, kind: RemoteCatalogKind) -> RemoteAction {
    RemoteAction::Query {
        query: RemoteQuery {
            query_id,
            kind,
            text: "original".into(),
            offset: 0,
        },
    }
}
fn start(operation_id: u64) -> RemoteAction {
    RemoteAction::Control {
        control: RemoteControl::StartOnDevice {
            operation_id: Some(operation_id),
            item: mapping::track(&track('1')).unwrap(),
            device_id: "fixture-device".into(),
        },
    }
}
fn received(server: &Server, accept: impl Fn(&Request) -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !server.records().iter().any(&accept) {
        assert!(
            Instant::now() < until,
            "Expected fixture request was not sent"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn retry_after_parser_rejects_invalid_values_and_preserves_long_limits() {
    assert_eq!(api::retry_after_seconds(" 7200 "), Some(7200));
    assert_eq!(api::retry_after_seconds("0"), Some(1));
    assert_eq!(
        api::retry_after_seconds("999999999999999999999999999999"),
        Some(u64::MAX)
    );
    for value in [
        "",
        "-1",
        "+1",
        "1.5",
        "NaN",
        "Wed, 21 Oct 2026 07:28:00 GMT",
    ] {
        assert_eq!(api::retry_after_seconds(value), None);
    }
}
#[test]
fn missing_retry_after_is_conservative_and_shared_across_http_workers() {
    let server = Server::new();
    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = None;
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let mut catalog = Api::new(server.base.clone(), shared.clone()).unwrap();
    catalog.token = Some("fixture-bearer".into());
    let mut playback = Api::new(server.base.clone(), shared).unwrap();
    playback.token = Some("fixture-bearer".into());
    let failure = catalog
        .call(reqwest::Method::GET, "me/tracks", &[], None)
        .err()
        .unwrap();
    assert_eq!(failure.retry_after, Some(30));
    assert_eq!(failure.status, 429);
    assert!(failure.error.message.contains("Try again in 30 seconds."));
    assert_eq!(playback.playback().err().unwrap().status, 429);
    assert_eq!(
        server.records().len(),
        1,
        "The other worker must not issue HTTP inside Retry-After"
    );
}
#[test]
fn quota_headers_block_other_http_worker_before_the_reason_is_read() {
    let server = Server::new();
    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = None;
    *server.rate_reply.lock().unwrap() = Some(
        r#"{"error":{"status":429,"reason":"QUOTA_EXCEEDED","message":"fixture-private-message"}}"#
            .into(),
    );
    server.hold_rate_body.store(true, Ordering::SeqCst);
    let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
    let catalog = Api::new(server.base.clone(), shared.clone()).unwrap();
    let playback = Api::new(server.base.clone(), shared.clone()).unwrap();
    let request = thread::spawn(move || catalog.call(reqwest::Method::GET, "me/tracks", &[], None));
    let deadline = Instant::now() + Duration::from_secs(3);
    while shared.0.lock().unwrap().rate(Instant::now()).is_none() {
        assert!(
            Instant::now() < deadline,
            "Spotify's headers did not set a shared cooldown"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let blocked = playback.playback().err().unwrap();
    assert_eq!(blocked.status, 429);
    assert_eq!(server.records().len(), 1);
    assert!(
        !request.is_finished(),
        "The fixture body must still be pending"
    );
    server.hold_rate_body.store(false, Ordering::SeqCst);
    let quota = request.join().unwrap().err().unwrap();
    assert_eq!(quota.retry_after, Some(30));
    assert_eq!(quota.error.code, ErrorCode::RateLimited);
    assert!(quota.error.message.contains("developer account"));
    assert!(!quota.error.message.contains("seconds"));
    assert!(!quota.error.message.contains("fixture-private-message"));
    assert_eq!(
        playback.playback().err().unwrap().error.message,
        quota.error.message
    );
    assert_eq!(server.records().len(), 1);
}

#[test]
fn unknown_and_oversized_rate_limit_bodies_keep_generic_feedback() {
    for body in [
        r#"{"error":{"reason":"OTHER_LIMIT","message":"QUOTA_EXCEEDED"}}"#.into(),
        r#"{"error":{"reason":"QUOTA_EXCEEDED""#.into(),
        format!(
            r#"{{"error":{{"reason":"QUOTA_EXCEEDED","message":"{}"}}}}"#,
            "x".repeat(8192)
        ),
    ] {
        let server = Server::new();
        server.mode.store(429, Ordering::SeqCst);
        *server.retry_after.lock().unwrap() = Some("60".into());
        *server.rate_reply.lock().unwrap() = Some(body);
        let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let api = Api::new(server.base.clone(), shared).unwrap();
        let failure = api.playback().err().unwrap();
        assert_eq!(failure.status, 429);
        assert_eq!(failure.retry_after, Some(60));
        assert!(failure.error.message.contains("Try again in 60 seconds."));
    }
}

#[test]
fn quota_survives_credentials_refresh_and_queries_finish_without_http_or_delayed_writes() {
    let server = Server::new();
    server.mode.store(429, Ordering::SeqCst);
    *server.retry_after.lock().unwrap() = Some("7200".into());
    *server.rate_reply.lock().unwrap() =
        Some(r#"{"error":{"status":429,"reason":"QUOTA_EXCEEDED"}}"#.into());
    let mut provider = server.provider();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    wait(&mut provider, |update| {
        matches!(update,RemoteUpdate::State(state) if state.retry_after_seconds.is_some_and(|n|n>3600)
            && state.issue.as_ref().is_some_and(|issue| issue.message.contains("developer account")))
    });
    let count = server.records().len();
    provider
        .submit(RemoteAction::Credential {
            access_token: "fixture-bearer".into(),
        })
        .unwrap();
    provider.submit(RemoteAction::Refresh).unwrap();
    provider
        .submit(query(12, RemoteCatalogKind::Search))
        .unwrap();
    let page = wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Page(page) if page.query_id==12),
    );
    assert!(
        matches!(page,RemoteUpdate::Page(page) if page.issue.as_ref().is_some_and(|issue| issue.code==ErrorCode::RateLimited
            && issue.message.contains("developer account")))
    );
    let detail_id = "a".repeat(22);
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 13,
                kind: RemoteCatalogKind::Track,
                text: detail_id,
                offset: 0,
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Page(page) if page.detail && page.query_id==13 && page.issue.as_ref().is_some_and(|error|error.code==ErrorCode::RateLimited)),
    );
    assert!(provider.shared.0.lock().unwrap().detail_credits.is_empty());
    assert_eq!(
        provider.submit(start(42)).unwrap_err().code,
        ErrorCode::RateLimited
    );
    assert_eq!(
        provider
            .submit(RemoteAction::Edit {
                request: RemoteEditRequest {
                    operation_id: 43,
                    edit: RemoteEdit::Library {
                        uris: vec![format!("spotify:track:{}", "a".repeat(22))],
                        save: true
                    }
                }
            })
            .unwrap_err()
            .code,
        ErrorCode::RateLimited
    );
    thread::sleep(Duration::from_millis(180));
    assert_eq!(
        server.records().len(),
        count,
        "Credential rotation and refresh cannot bypass the shared quota"
    );
    server.mode.store(200, Ordering::SeqCst);
    provider.shared.0.lock().unwrap().rate_limit = Some(Cooldown::new(Instant::now(), 0));
    provider.shared.1.notify_all();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::State(state) if state.connection==ProviderConnectionState::Connected),
    );
    assert!(
        server.records()[count..]
            .iter()
            .all(|request| request.method == "GET"),
        "Rejected actions must not run after recovery"
    );
}
#[test]
fn quota_cancels_queued_catalog_details_and_edit_without_stale_main_page_overwrite() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.mode.store(429, Ordering::SeqCst);
    server.delay.store(180, Ordering::SeqCst);
    provider
        .submit(query(11, RemoteCatalogKind::Search))
        .unwrap();
    received(&server, |request| request.target.starts_with("/v1/search"));
    provider
        .submit(query(12, RemoteCatalogKind::Search))
        .unwrap();
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 13,
                kind: RemoteCatalogKind::Track,
                text: "a".repeat(22),
                offset: 0,
            },
        })
        .unwrap();
    provider
        .submit(RemoteAction::Edit {
            request: RemoteEditRequest {
                operation_id: 88,
                edit: RemoteEdit::Library {
                    uris: vec![format!("spotify:track:{}", "a".repeat(22))],
                    save: true,
                },
            },
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let (mut main, mut detail, mut edited) = (false, false, false);
    while !(main && detail && edited) {
        assert!(
            Instant::now() < until,
            "Queued work did not finish under quota"
        );
        if let Some(update) = provider.poll() {
            match update {
                RemoteUpdate::Page(page) => {
                    assert_eq!(page.issue.as_ref().unwrap().code, ErrorCode::RateLimited);
                    if page.detail {
                        assert_eq!(page.query_id, 13);
                        detail = true;
                    } else {
                        assert_eq!(
                            page.query_id, 12,
                            "Old in-flight query overwrote the latest failed page"
                        );
                        main = true;
                    }
                }
                RemoteUpdate::Edited(result) => {
                    assert_eq!(result.operation_id, 88);
                    assert_eq!(result.error.unwrap().code, ErrorCode::RateLimited);
                    edited = true;
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let mailbox = provider.shared.0.lock().unwrap();
    assert!(mailbox.detail_credits.is_empty() && !mailbox.edit_busy);
    drop(mailbox);
    assert_eq!(
        server
            .records()
            .iter()
            .filter(|request| request.target.starts_with("/v1/search"))
            .count(),
        1
    );
    assert!(
        !server
            .records()
            .iter()
            .any(|request| request.method != "GET" || request.target.starts_with("/v1/tracks/"))
    );
}
#[test]
fn queued_controls_do_not_play_after_network_backoff_and_explicit_retry_only_reads() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    server.mode.store(294, Ordering::SeqCst);
    server.delay.store(180, Ordering::SeqCst);
    provider.submit(start(41)).unwrap();
    received(&server, |request| request.method != "GET");
    provider
        .submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
        .unwrap();
    provider.submit(start(43)).unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::State(state) if state.connection==ProviderConnectionState::Unavailable && state.control_result.as_ref().is_some_and(|result|result.operation_id==43 && result.error.is_some())),
    );
    assert_eq!(
        provider.submit(start(45)).unwrap_err().code,
        ErrorCode::NetworkError
    );
    server.mode.store(200, Ordering::SeqCst);
    server.delay.store(0, Ordering::SeqCst);
    provider.submit(RemoteAction::Refresh).unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::State(state) if state.connection==ProviderConnectionState::Connected),
    );
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        server
            .records()
            .iter()
            .filter(|request| request.method != "GET")
            .count(),
        1,
        "Old Next/Start intents were replayed after the network recovered"
    );
}
#[test]
fn an_inflight_success_cannot_hide_quota_and_expiry_requires_a_fresh_observation() {
    let now = Instant::now();
    let mut projection = output_clock::OutputProjection::default();
    projection.ingest(output_base(1), now);
    projection.take(now);
    projection.observe(Some(output_clock(1)), now).unwrap();
    projection.take(now);
    projection.set_rate_limit(Some(RateLimit {
        seconds: 2,
        developer_quota: true,
    }));
    assert_eq!(
        projection.take(now).unwrap().connection,
        ProviderConnectionState::Unavailable
    );
    projection.ingest(output_base(2), now);
    let during = projection.take(now).unwrap();
    assert_eq!(during.retry_after_seconds, Some(2));
    let issue = during.issue.unwrap();
    assert_eq!(issue.code, ErrorCode::RateLimited);
    assert!(issue.message.contains("developer account"));
    projection.set_rate_limit(Some(RateLimit {
        seconds: 1,
        developer_quota: true,
    }));
    assert_eq!(projection.take(now).unwrap().retry_after_seconds, Some(1));
    projection.set_rate_limit(None);
    assert_eq!(
        projection.take(now).unwrap().connection,
        ProviderConnectionState::Unavailable
    );
    projection.ingest(output_base(3), now);
    assert_eq!(
        projection.take(now).unwrap().connection,
        ProviderConnectionState::Connected
    );
}
#[test]
fn catalog_not_found_is_not_reported_as_a_missing_output_device() {
    let server = Server::new();
    server.mode.store(404, Ordering::SeqCst);
    let mut api = fixture_api(&server);
    let playback = api.playback().err().unwrap();
    assert!(playback.error.message.contains("device"));
    let playlist = api
        .catalog(&RemoteQuery {
            query_id: 1,
            kind: RemoteCatalogKind::PlaylistInfo,
            text: "p".repeat(22),
            offset: 0,
        })
        .err()
        .unwrap();
    assert!(
        playlist.error.message.contains("playlist") && !playlist.error.message.contains("device")
    );
    server.mode.store(403, Ordering::SeqCst);
    let snapshot = api
        .catalog(&RemoteQuery {
            query_id: 2,
            kind: RemoteCatalogKind::PlaylistItems,
            text: "p".repeat(22),
            offset: 0,
        })
        .err()
        .unwrap();
    assert!(snapshot.error.message.contains("owned or collaborative"));
}

#[test]
fn a_main_query_rejected_under_quota_still_revokes_old_positional_context() {
    let server = Server::new();
    let mut provider = server.provider();
    connect(&mut provider);
    provider
        .submit(RemoteAction::Query {
            query: RemoteQuery {
                query_id: 7,
                kind: RemoteCatalogKind::PlaylistItems,
                text: "p".repeat(22),
                offset: 0,
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Page(page) if page.query_id==7 && page.position_edits),
    );
    provider
        .shared
        .0
        .lock()
        .unwrap()
        .limit_requests(30, Instant::now());
    provider
        .submit(query(8, RemoteCatalogKind::Search))
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Page(page) if page.query_id==8 && page.issue.is_some()),
    );
    provider.shared.0.lock().unwrap().rate_limit = Some(Cooldown::new(Instant::now(), 0));
    provider
        .submit(RemoteAction::Edit {
            request: RemoteEditRequest {
                operation_id: 90,
                edit: RemoteEdit::PlaylistMove {
                    id: "p".repeat(22),
                    query_id: 7,
                    from: 0,
                    to: 2,
                },
            },
        })
        .unwrap();
    wait(
        &mut provider,
        |update| matches!(update,RemoteUpdate::Edited(result) if result.operation_id==90 && result.error.as_ref().is_some_and(|error|error.code==ErrorCode::CatalogChanged)),
    );
    assert!(
        !server
            .records()
            .iter()
            .any(|request| request.method == "PUT")
    );
}
