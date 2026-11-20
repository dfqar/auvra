use super::*;

fn chapter(id: char) -> Value {
    let mut row = spoken_progress_fixture(
        "episode",
        id,
        json!({"resume_position_ms":41000,"fully_played":false}),
    );
    row.as_object_mut().unwrap().remove("show");
    row["chapter_number"] = json!(1);
    row["uri"] = json!(format!("spotify:episode:{}", id.to_string().repeat(22)));
    row
}

fn chapter_variant(id: char) -> Value {
    let mut row = chapter(id);
    match id {
        'l' => {
            row["type"] = json!("chapter");
            row.as_object_mut().unwrap().remove("uri");
        }
        'n' => {
            row.as_object_mut().unwrap().remove("type");
            row.as_object_mut().unwrap().remove("uri");
        }
        'p' => {
            row["is_playable"] = json!(false);
            row["restrictions"] = json!({"reason":"payment_required"});
        }
        't' => row["type"] = json!("track"),
        'u' => row["uri"] = json!(format!("spotify:episode:{}", "m".repeat(22))),
        'x' => {
            row = spoken_progress_fixture(
                "episode",
                id,
                json!({"resume_position_ms":23000,"fully_played":false}),
            );
            row.as_object_mut().unwrap().remove("audiobook");
            row["uri"] = json!(format!("spotify:episode:{}", id.to_string().repeat(22)));
        }
        _ => {}
    }
    row
}

pub(super) fn chapter_detail(path: &str) -> Option<Value> {
    let id = path.strip_prefix("/v1/chapters/")?.chars().next()?;
    match id {
        'i' => Some(chapter('d')), // Correct chapter shape for a different ID.
        'd' | 'l' | 'n' | 'p' | 't' | 'u' | 'x' => Some(chapter_variant(id)),
        _ => None,
    }
}

pub(super) fn chapter_page(path: &str) -> Option<Value> {
    if path != format!("/v1/audiobooks/{}/chapters", "d".repeat(22)) {
        return None;
    }
    let rows = ['d', 't', 'l', 'u', 'n', 'x', 'p'].map(|id| {
        let mut row = chapter_variant(id);
        // The list uses simplified chapter objects.
        row.as_object_mut().unwrap().remove("audiobook");
        row
    });
    Some(json!({"total":17,"offset":10,"items":rows}))
}

fn read(api: &mut Api, kind: RemoteCatalogKind, id: &str, offset: u32) -> RemotePage {
    api.catalog(&RemoteQuery {
        query_id: 1,
        kind,
        text: id.repeat(22),
        offset,
    })
    .unwrap_or_else(|_| panic!("chapter catalog fixture"))
}

#[test]
fn chapter_detail_accepts_documented_episode_wire_type_and_legacy_shapes() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    for id in ["d", "l", "n"] {
        let page = read(&mut api, RemoteCatalogKind::Chapter, id, 0);
        assert!(page.issue.is_none());
        let [item] = page.items.as_slice() else {
            panic!("expected one valid chapter");
        };
        let logical_uri = format!("spotify:chapter:{}", id.repeat(22));
        assert_eq!(item.id.as_str(), logical_uri);
        assert!(matches!(
            &item.location,
            Some(MediaLocation::Remote { uri, .. }) if *uri == logical_uri
        ));
        assert_eq!(item.artists, ["Original author"]);
        assert_eq!(item.album.as_deref(), Some("Original book"));
        assert!(item.capabilities.play && !item.capabilities.library_save);
        assert_eq!(
            mapping::track_uri(item).unwrap(),
            format!("spotify:episode:{}", id.repeat(22))
        );
        assert!(mapping::queue_uri(item).is_err());
        assert_eq!(
            page.spoken_progress,
            [SpokenProgress {
                media_id: item.id.clone(),
                position_ms: 41_000,
                fully_played: false
            }]
        );
        assert_eq!(
            page.related_collections[0].kind,
            RemoteEntityKind::Audiobook
        );
    }
    let records = server.records();
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|r| r.method == "GET" && r.authorized));
}

#[test]
fn chapter_routes_omit_mismatches_and_preserve_page_positions_and_restrictions() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    for id in ["i", "t", "u", "x"] {
        let page = read(&mut api, RemoteCatalogKind::Chapter, id, 0);
        assert!(page.items.is_empty());
        assert!(page.spoken_progress.is_empty());
        assert!(page.related_collections.is_empty());
        assert_eq!(page.issue.unwrap().code, ErrorCode::TrackUnavailable);
    }
    let page = read(&mut api, RemoteCatalogKind::AudiobookChapters, "d", 10);
    assert_eq!(page.item_positions, [10, 12, 14, 16]);
    assert_eq!(page.total, 17);
    assert_eq!(page.issue.unwrap().code, ErrorCode::TrackUnavailable);
    assert_eq!(page.spoken_progress.len(), 4);
    for (item, id) in page.items.iter().zip(["d", "l", "n", "p"]) {
        assert_eq!(
            item.id.as_str(),
            format!("spotify:chapter:{}", id.repeat(22))
        );
        assert_eq!(item.capabilities.play, id != "p");
        assert!(!item.capabilities.library_save);
        assert!(item.artists.is_empty());
    }
}

#[test]
fn podcast_episode_routes_keep_episode_identity_progress_and_parent() {
    let server = Server::new();
    let mut api = fixture_api(&server);
    let page = read(&mut api, RemoteCatalogKind::Episode, "e", 0);
    let item = &page.items[0];
    assert_eq!(
        item.id.as_str(),
        format!("spotify:episode:{}", "e".repeat(22))
    );
    assert_eq!(item.artists, ["Original publisher"]);
    assert!(item.capabilities.library_save && mapping::queue_uri(item).is_ok());
    assert_eq!(page.spoken_progress[0].media_id, item.id);
    assert_eq!(page.related_collections[0].kind, RemoteEntityKind::Show);
}
