use super::*;

pub(super) fn saved_page(path: &str, membership: &HashSet<String>) -> Value {
    let (kind, id) = match path {
        "/v1/me/shows" => ("show", "s".repeat(22)),
        "/v1/me/episodes" => ("episode", "e".repeat(22)),
        "/v1/me/audiobooks" => ("audiobook", "b".repeat(22)),
        _ => unreachable!(),
    };
    if !membership.contains(&format!("spotify:{kind}:{id}")) {
        return json!({"total":0,"items":[]});
    }
    let mut item = if kind == "episode" {
        let mut episode = spoken_fixture(kind);
        episode.as_object_mut().unwrap().remove("audiobook");
        episode
    } else {
        collection_fixture(kind)
    };
    item["id"] = json!(id);
    let row = match kind {
        "show" => json!({"added_at":"2026-11-06T09:00:00Z","show":item}),
        "episode" => json!({"added_at":"2026-11-06T09:00:00Z","episode":item}),
        _ => item,
    };
    json!({"total":1,"items":[row]})
}

#[test]
fn spoken_library_membership_round_trips_through_spotify_http() {
    let server = Server::new();
    server.stateful_spoken_library.store(true, Ordering::SeqCst);
    let mut provider = server.provider();
    connect(&mut provider);

    for (index, (kind, entity, uri_type, id, path)) in [
        (
            RemoteCatalogKind::SavedShows,
            Some(RemoteEntityKind::Show),
            "show",
            "s".repeat(22),
            "/v1/me/shows",
        ),
        (
            RemoteCatalogKind::SavedEpisodes,
            None,
            "episode",
            "e".repeat(22),
            "/v1/me/episodes",
        ),
        (
            RemoteCatalogKind::SavedAudiobooks,
            Some(RemoteEntityKind::Audiobook),
            "audiobook",
            "b".repeat(22),
            "/v1/me/audiobooks",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let before = server.records().len();
        let uri = format!("spotify:{uri_type}:{id}");
        let uris = vec![uri.clone()];
        let mut operation_id = (index as u64) * 10;
        let mut edit = |provider: &mut SpotifyRemoteProvider, edit| {
            operation_id += 1;
            provider
                .submit(RemoteAction::Edit {
                    request: RemoteEditRequest { operation_id, edit },
                })
                .unwrap();
            let RemoteUpdate::Edited(result) = wait(
                provider,
                |update| matches!(update, RemoteUpdate::Edited(result) if result.operation_id == operation_id),
            ) else {
                unreachable!()
            };
            assert!(result.error.is_none(), "{result:?}");
            result.saved
        };
        let read = |provider: &mut SpotifyRemoteProvider, query_id| {
            provider
                .submit(RemoteAction::Query {
                    query: RemoteQuery {
                        query_id,
                        kind: kind.clone(),
                        text: String::new(),
                        offset: 0,
                    },
                })
                .unwrap();
            let RemoteUpdate::Page(page) = wait(
                provider,
                |update| matches!(update, RemoteUpdate::Page(page) if page.query_id == query_id),
            ) else {
                unreachable!()
            };
            assert!(page.issue.is_none(), "{page:?}");
            page
        };
        let assert_membership = |status: Vec<RemoteSavedItem>, expected| {
            let [membership] = status.as_slice() else {
                panic!("expected one membership result for {uri}");
            };
            assert_eq!(membership.uri, uri);
            assert_eq!(membership.saved, expected);
        };
        let check = || RemoteEdit::CheckLibrary { uris: uris.clone() };
        let save = |save| RemoteEdit::Library {
            uris: uris.clone(),
            save,
        };

        assert_membership(edit(&mut provider, check()), false);
        assert!(edit(&mut provider, save(true)).is_empty());
        let page = read(&mut provider, 100 + index as u64 * 2);
        assert_eq!(page.total, 1);
        if let Some(entity) = entity {
            let [collection] = page.collections.as_slice() else {
                panic!("missing {uri}")
            };
            assert_eq!(collection.kind, entity);
            assert_eq!(collection.id, id);
            assert!(page.items.is_empty());
        } else {
            let [episode] = page.items.as_slice() else {
                panic!("missing {uri}")
            };
            assert_eq!(episode.id.as_str(), uri);
            assert!(episode.capabilities.library_save);
            assert!(page.collections.is_empty() && page.spoken_progress.is_empty());
        }
        assert_membership(edit(&mut provider, check()), true);
        assert!(edit(&mut provider, save(false)).is_empty());
        let removed = read(&mut provider, 101 + index as u64 * 2);
        assert_eq!(removed.total, 0);
        assert!(removed.items.is_empty() && removed.collections.is_empty());
        assert_membership(edit(&mut provider, check()), false);

        let library = "/v1/me/library";
        let contains = "/v1/me/library/contains";
        let requests = server.records();
        let relevant = requests[before..]
            .iter()
            .filter_map(|request| {
                let url =
                    reqwest::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
                (request.method != "GET" || [path, contains].contains(&url.path()))
                    .then_some((request, url))
            })
            .collect::<Vec<_>>();
        assert_eq!(relevant.len(), 7);
        for ((request, url), (method, target)) in relevant.iter().zip([
            ("GET", contains),
            ("PUT", library),
            ("GET", path),
            ("GET", contains),
            ("DELETE", library),
            ("GET", path),
            ("GET", contains),
        ]) {
            assert_eq!(request.method, method);
            assert_eq!(url.path(), target);
            assert!(request.authorized);
            if target != path {
                assert_eq!(
                    url.query_pairs().into_owned().collect::<Vec<_>>(),
                    [("uris".into(), uri.clone())]
                );
            }
        }
    }
}
