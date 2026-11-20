use super::*;
#[test]
fn lrc_repeats_offset_unicode_and_breaks() {
    let d = parse(
        "lrc",
        "[offset:-500]\n[00:01.00][00:03.00]Тихий берег 🌊\n[00:04.00]\n[00:07]Домой",
        10000,
    )
    .unwrap();
    assert_eq!(d.precision, "line");
    assert_eq!(d.lines.len(), 4);
    assert_eq!(d.lines[0].start_ms, Some(500));
    assert_eq!(d.lines[0].end_ms, Some(2500));
    assert_eq!(d.lines[2].text, "");
    assert_eq!(d.lines[3].end_ms, Some(10000));
}
#[test]
fn lrc_preserves_literal_angle_brackets_and_line_timing() {
    let d = parse("lrc", "[00:01]I <3 you\n[00:04]<Soft echo>", 9000).unwrap();
    assert_eq!(d.precision, "line");
    assert_eq!(d.lines[0].text, "I <3 you");
    assert_eq!(d.lines[0].start_ms, Some(1000));
    assert_eq!(d.lines[0].end_ms, Some(4000));
    assert!(d.lines[0].words.is_empty());
    assert_eq!(d.lines[1].text, "<Soft echo>");
}
#[test]
fn enhanced_lrc_preserves_sustained_word() {
    let d = parse("lrc", "[00:01]  <00:01>Stay <00:02>here<00:06>", 9000).unwrap();
    assert_eq!(d.precision, "word");
    assert_eq!(d.lines[0].text, "Stay here");
    assert_eq!(
        d.lines[0].words[1].end_ms - d.lines[0].words[1].start_ms,
        4000
    );
}
#[test]
fn lrc_bom_preserves_first_timestamp_and_positive_offset() {
    let first_cue = parse(
        "lrc",
        "\u{feff}[00:01] First line\n[00:03]Second line",
        9000,
    )
    .unwrap();
    assert_eq!(first_cue.lines.len(), 2);
    assert_eq!(first_cue.lines[0].start_ms, Some(1000));
    let offset = parse("lrc", "\u{feff}[offset:+250]\n[00:01] First line", 9000).unwrap();
    assert_eq!(offset.lines[0].start_ms, Some(1250));
}
#[test]
fn enhanced_lrc_downgrades_only_the_lines_with_broken_word_timing() {
    let d = parse(
        "lrc",
        "[00:01]<00:01>Unknown end\n[00:02]<00:invalid>Unknown start\n[00:03]<00:03>Word<00:06\n[00:04]<00:05>Back <00:04>wards<00:06>\n[00:07] <00:07>Stay <00:08>here <3<00:09>",
        10_000,
    )
    .unwrap();
    assert_eq!(d.precision, "word");
    for (index, expected) in ["Unknown end", "Unknown start", "Word", "Back wards"]
        .into_iter()
        .enumerate()
    {
        let line = &d.lines[index];
        assert_eq!(line.text, expected);
        assert_eq!(line.start_ms, Some((index as u64 + 1) * 1000));
        assert!(line.words.is_empty());
    }
    assert_eq!(d.lines[0].end_ms, Some(2000));
    assert_eq!(d.lines[4].text, "Stay here <3");
    assert_eq!(d.lines[4].words[1].end_ms, 9000);
}
#[test]
fn enhanced_lrc_keeps_repeated_line_starts_without_copying_word_times() {
    let d = parse("lrc", "[00:01][00:05]<00:01>Stay <00:02>here<00:03>", 9000).unwrap();
    assert_eq!(d.precision, "line");
    assert_eq!(d.lines.len(), 2);
    assert_eq!(d.lines[0].start_ms, Some(1000));
    assert_eq!(d.lines[1].start_ms, Some(5000));
    assert!(
        d.lines
            .iter()
            .all(|line| line.text == "Stay here" && line.words.is_empty())
    );
}
#[test]
fn untimed_lrc_preserves_text_without_inventing_cues() {
    let d = parse(
        "lrc",
        "\u{feff}[ar:Original artist]\n[#:Editorial note]\n[Verse]\nF: First line\n\nD: Second line",
        9000,
    )
    .unwrap();
    assert_eq!(d.precision, "plain");
    assert_eq!(
        d.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        ["[Verse]", "F: First line", "", "D: Second line"]
    );
    assert!(
        d.lines
            .iter()
            .all(|line| line.start_ms.is_none() && line.end_ms.is_none() && line.words.is_empty())
    );
    assert!(parse("lrc", "[00:invalid]Bad line", 9000).is_err());
    assert!(parse("lrc", "<00:01>Unknown line", 9000).is_err());
}
#[test]
fn lrc_checks_repeated_output_before_expanding_it() {
    let too_many_cues = "[00:01]".repeat(3001) + "A";
    assert_eq!(
        parse("lrc", &too_many_cues, 9000),
        Err("Too many lyrics cues")
    );
    let too_much_text = "[00:01]".repeat(3000) + &"A".repeat(4096);
    assert_eq!(
        parse("lrc", &too_much_text, 9000),
        Err("Lyrics exceed supported limits")
    );
}
#[test]
fn yaml_real_intervals_and_overlap() {
    let d = parse("lyricsfile", "version: '1.0'\nlines:\n  - text: 'Stay here'\n    start_ms: 1000\n    end_ms: 6000\n    words:\n      - {text: 'Stay ', start_ms: 1000, end_ms: 2000}\n      - {text: here, start_ms: 2000, end_ms: 6000}\n  - {text: 'Backing voice', start_ms: 4000, end_ms: 7000}\n", 9000).unwrap();
    assert_eq!(d.precision, "word");
    assert!(!d.lines[0].words[1].end_estimated);
    assert_eq!(d.lines[1].start_ms, Some(4000));
}
#[test]
fn yaml_start_only_is_marked_estimated() {
    let d = parse("yaml", "version: '1.0'\nlines:\n - text: 'One two'\n   start_ms: 100\n   end_ms: 900\n   words:\n    - {text: 'One ', start_ms: 100}\n    - {text: two, start_ms: 500}\n", 1000).unwrap();
    assert!(d.lines[0].words.iter().all(|w| w.end_estimated));
}
#[test]
fn ttml_relative_timing_and_nested_styles() {
    let d = parse("ttml", r#"<tt xmlns="http://www.w3.org/ns/ttml"><body begin="1s"><div><p begin="1s" end="8s"><span begin="0s" end="1s">Come </span><span begin="1s" end="6s"><span>home</span></span></p></div></body></tt>"#, 10000).unwrap();
    assert_eq!(d.precision, "word");
    assert_eq!(d.lines[0].text, "Come home");
    assert_eq!(d.lines[0].start_ms, Some(2000));
    assert_eq!(d.lines[0].words[1].end_ms, 8000);
}

#[test]
fn ttml_ignores_paragraph_metadata_without_dropping_word_timing() {
    let text = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:edit="urn:fixture:editor"><body><div><p begin="0s" end="8s"><metadata><ttm:desc>Editorial note</ttm:desc><edit:annotation edit:ruby="editor-only">A second note</edit:annotation></metadata><span begin="0s" end="2s">Carry </span><span begin="2s" end="8s"><metadata><ttm:desc>Hold for six seconds</ttm:desc></metadata>the light</span></p></div></body></tt>"#;
    let dfxp = text.replace(
        "http://www.w3.org/ns/ttml",
        "http://www.w3.org/2006/10/ttaf1",
    );
    for format in ["ttml", "ttml-amll"] {
        let document = parse(format, text, 8000).unwrap();
        assert_eq!(parse(format, &dfxp, 8000).unwrap(), document);
        assert_eq!(document.precision, "word");
        assert_eq!(document.lines.len(), 1);
        let line = &document.lines[0];
        assert_eq!(line.text, "Carry the light");
        assert_eq!(line.words.len(), 2);
        assert_eq!(line.words[0].text, "Carry ");
        assert_eq!(line.words[1].text, "the light");
        assert_eq!(line.words[1].start_ms, 2000);
        assert_eq!(line.words[1].end_ms, 8000);
    }
}

#[test]
fn ttml_selects_body_paragraphs_by_namespace_and_preserves_bodyless_input() {
    let text = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:edit="urn:fixture:editor"><head><metadata><edit:p begin="invalid">Editorial note</edit:p></metadata></head><edit:body><edit:p begin="invalid">Foreign body</edit:p></edit:body><body><metadata><edit:p begin="invalid">Another note</edit:p></metadata><p begin="2s" end="5s">The actual lyric</p></body></tt>"#;
    for format in ["ttml", "ttml-amll"] {
        let document = parse(format, text, 8000).unwrap();
        assert_eq!(document.lines.len(), 1);
        assert_eq!(document.lines[0].text, "The actual lyric");
        assert_eq!(document.lines[0].start_ms, Some(2000));
    }
    let bodyless = r#"<tt xmlns:edit="urn:fixture:editor"><head><metadata><edit:p begin="invalid">Editorial note</edit:p></metadata></head><p begin="2s" end="5s">The actual lyric</p></tt>"#;
    assert_eq!(
        parse("ttml", bodyless, 8000).unwrap(),
        parse("ttml", text, 8000).unwrap()
    );
}

#[test]
fn ttml_preserves_layout_breaks_and_original_word_cues() {
    let text = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="0s" end="7s">
        <br/> <span begin="0s" end="2s">River</span><br/> <br/>
        <span begin="2s" end="7s">sing<br/>ing</span><br/>   </p>
    </div></body></tt>"#;
    for format in ["ttml", "ttml-amll"] {
        let document = parse(format, text, 7000).unwrap();
        let line = &document.lines[0];
        assert_eq!(document.precision, "word");
        assert_eq!(line.text, "\nRiver\n\nsing\ning\n");
        assert_eq!(line.words.len(), 2);
        assert_eq!(line.words[0].text, "\nRiver\n\n");
        assert_eq!(line.words[1].text, "sing\ning\n");
        assert_eq!(line.words[1].start_ms, 2000);
        assert_eq!(line.words[1].end_ms, 7000);
        assert!(!line.words[1].end_estimated);

        let mixed = text.replace("<br/>   ", "<br/>Untimed vocals");
        let mixed = parse(format, &mixed, 7000).unwrap();
        assert_eq!(mixed.precision, "line");
        assert_eq!(mixed.lines[0].text, "\nRiver\n\nsing\ning\nUntimed vocals");
        assert!(mixed.lines[0].words.is_empty());
    }
}
#[test]
fn ttml_rejects_external_entities_bad_intervals_and_seq() {
    for text in [
        r#"<!DOCTYPE tt [<!ENTITY x SYSTEM "file:///etc/passwd">]><tt><p>&x;</p></tt>"#,
        r#"<tt><p begin="2s" end="1s">bad</p></tt>"#,
        r#"<tt><body timeContainer="seq"><p>bad</p></body></tt>"#,
    ] {
        assert!(parse("ttml", text, 9000).is_err());
    }
}

#[test]
fn amll_absolute_words_preserve_sustained_timing_and_ignore_auxiliary_tracks() {
    let text = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><head><metadata><p begin="0" end="1">Metadata is not a lyric</p></metadata></head><body dur="10"><div begin="2" end="9"><p begin="2" end="8.1"><span begin="2" end="4">Our </span><span begin="4" end="8.1">light</span><span ttm:role="x-translation">Not sung</span><span ttm:role="x-roman">Not sung either</span></p></div></body></tt>"#;
    let d = parse("ttml-amll", text, 20_000).unwrap();
    assert_eq!(d.precision, "word");
    assert_eq!(d.lines.len(), 1);
    assert_eq!(d.lines[0].text, "Our light");
    assert_eq!(d.lines[0].start_ms, Some(2000));
    assert_eq!(d.lines[0].words[1].start_ms, 4000);
    assert_eq!(d.lines[0].words[1].end_ms, 8100);
    assert!(!d.lines[0].words[1].end_estimated);
    let no_body_duration = parse("ttml-amll", &text.replace(r#" dur="10""#, ""), 20_000).unwrap();
    assert_eq!(no_body_duration.lines[0].words[1].end_ms, 8100);
}

#[test]
fn amll_rejects_excess_duration_bad_absolute_bounds_and_unsupported_ruby() {
    let original = r#"<tt><body dur="00:10.000"><div begin="00:02.000" end="00:09.000"><p begin="00:02.000" end="00:08.000"><span begin="00:02.000" end="00:08.000">Auvra fixture</span></p></div></body></tt>"#;
    assert!(parse("ttml-amll", original, 10000).is_ok());
    assert!(parse("ttml-amll", original, 9999).is_err());
    let no_body_duration = original.replace(r#" dur="00:10.000""#, "");
    assert!(parse("ttml-amll", &no_body_duration, 7999).is_err());
    for text in [
        original.replace("dur=\"00:10.000\"", "dur=\"invalid\""),
        original.replace("<body dur=\"00:10.000\"", "<body dur=\"00:08.999\""),
        original.replace("<p begin=\"00:02.000\"", "<p begin=\"00:01.000\""),
        original.replace("<span begin=\"00:02.000\"", "<span begin=\"00:01.999\""),
        original.replace("end=\"00:08.000\">Auvra", "end=\"00:08.001\">Auvra"),
        original.replace(
            "<span begin",
            "<span xmlns:s=\"http://www.w3.org/ns/ttml#styling\" s:ruby=\"text\" begin",
        ),
        original.replace("<div begin", "<div timeContainer=\"seq\" begin"),
        format!(r#"<!DOCTYPE tt [<!ENTITY x SYSTEM "file:///etc/passwd">]>{original}"#),
    ] {
        assert!(parse("ttml-amll", &text, 10000).is_err());
    }
}

#[test]
fn amll_preserves_overlapping_vocals_without_clipping_or_reordering_words() {
    let text = r#"<tt><body dur="10"><p begin="2" end="8.1"><span begin="2" end="4.03">Our </span><span begin="4" end="8.1">light</span></p></body></tt>"#;
    let d = parse("ttml-amll", text, 10000).unwrap();
    assert_eq!(d.lines[0].words[0].end_ms, 4030);
    assert_eq!(d.lines[0].words[1].start_ms, 4000);
    assert!(!d.lines[0].words[0].end_estimated);
    assert!(
        parse(
            "ttml-amll",
            &text.replace("<span begin=\"4\"", "<span begin=\"1.99\""),
            10000
        )
        .is_err()
    );
}

#[test]
fn amll_keeps_overlapping_background_separate_from_the_lead_and_duet() {
    let text = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body dur="10"><div begin="1" end="9.5"><p ttm:agent="v1" begin="2.5" end="8"><span ttm:role="x-bg" begin="2" end="3"><span begin="2" end="3">(echo)</span></span><span begin="2.5" end="4">Bring</span><span begin="00:00.000" end="00:00.000"> </span><span begin="5" end="8">the light</span><span ttm:role="x-bg" begin="4.5" end="8.5"><span begin="4.5" end="6">(stay</span><span begin="6" end="6"> </span><span begin="6" end="8.5">near)</span><span ttm:role="x-translation">Translation</span><span ttm:role="x-roman">Transliteration</span></span><span ttm:role="x-bg" begin="8.5" end="9">(hush)</span><span ttm:role="x-bg">(whisper)</span></p><p ttm:agent="v2" begin="4.6" end="7"><span begin="4.6" end="7">Sing again</span></p></div></body></tt>"#;
    let document = parse("ttml-amll", text, 10000).unwrap();
    assert_eq!(document.precision, "word");
    assert_eq!(document.lines.len(), 2);
    let lead = &document.lines[0];
    assert_eq!(lead.text, "Bring the light");
    assert_eq!((lead.start_ms, lead.end_ms), (Some(2500), Some(8000)));
    assert_eq!(
        lead.words
            .iter()
            .map(|word| (
                word.text.as_str(),
                word.start_ms,
                word.end_ms,
                word.end_estimated
            ))
            .collect::<Vec<_>>(),
        [
            ("Bring ", 2500, 4000, false),
            ("the light", 5000, 8000, false)
        ]
    );
    assert_eq!(lead.background.len(), 4);
    let before = &lead.background[0];
    assert_eq!((before.start_ms, before.end_ms), (Some(2000), Some(3000)));
    assert_eq!(before.words[0].text, "(echo)");
    let overlap = &lead.background[1];
    assert_eq!((overlap.start_ms, overlap.end_ms), (Some(4500), Some(8500)));
    assert_eq!(overlap.text, "(stay near)");
    assert_eq!(
        overlap
            .words
            .iter()
            .map(|word| (word.text.as_str(), word.start_ms, word.end_ms))
            .collect::<Vec<_>>(),
        [("(stay ", 4500, 6000), ("near)", 6000, 8500)]
    );
    let line_only = &lead.background[2];
    assert_eq!(line_only.text, "(hush)");
    assert_eq!(
        (line_only.start_ms, line_only.end_ms),
        (Some(8500), Some(9000))
    );
    let untimed = &lead.background[3];
    assert_eq!(untimed.text, "(whisper)");
    assert_eq!((untimed.start_ms, untimed.end_ms), (None, None));
    assert!(line_only.words.is_empty() && untimed.words.is_empty());
    assert!(
        lead.background
            .iter()
            .all(|line| line.background.is_empty())
    );
    let duet = &document.lines[1];
    assert_eq!((duet.start_ms, duet.end_ms), (Some(4600), Some(7000)));
    assert_eq!(duet.words[0].text, "Sing again");
    assert!(duet.background.is_empty());

    let inferred = text.replace(
        r#"ttm:role="x-bg" begin="4.5" end="8.5""#,
        r#"ttm:role="x-bg""#,
    );
    assert_eq!(parse("ttml-amll", &inferred, 10000).unwrap(), document);
    let json = serde_json::to_value(&document).unwrap();
    assert!(json["lines"][0]["background"].is_array());
    assert!(json["lines"][1].get("background").is_none());
    assert_eq!(serde_json::from_value::<Document>(json).unwrap(), document);
}

#[test]
fn amll_background_and_layout_separators_keep_strict_source_limits() {
    let track =
        r#"<span ttm:role="x-bg" begin="4" end="7"><span begin="4.1" end="7">(echo)</span></span>"#;
    let fixture = |backing: &str| {
        format!(
            r#"<tt xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body dur="10"><div begin="1" end="8"><p begin="2" end="6"><span begin="2" end="3">A</span><span begin="0" end="0"> </span><span begin="4" end="6">voice</span>{backing}</p></div></body></tt>"#
        )
    };
    assert!(parse("ttml-amll", &fixture(track), 10000).is_ok());
    for invalid in [
        fixture(&track.replace(r#"begin="4""#, r#"begin="0.9""#)),
        fixture(&track.replace(r#"end="7""#, r#"end="8.1""#)),
        fixture(&track.replace(r#"begin="4.1""#, r#"begin="3.9""#)),
        fixture(&track.replace("(echo)", &format!("(echo){track}"))),
        fixture(&track.repeat(9)),
        fixture(track).replace(
            r#"begin="0" end="0"> </span>"#,
            r#"begin="0" end="0">words</span>"#,
        ),
        fixture(track).replace(
            r#"begin="0" end="0"> </span>"#,
            r#"begin="0" end="1"> </span>"#,
        ),
    ] {
        assert!(parse("ttml-amll", &invalid, 10000).is_err(), "{invalid}");
    }
    let rows = r#"<p begin="1" end="2">A<span ttm:role="x-bg">B</span></p>"#.repeat(1501);
    let over_budget =
        format!(r#"<tt xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body>{rows}</body></tt>"#);
    assert!(parse("ttml-amll", &over_budget, 10000).is_err());
}

#[test]
fn plain_and_instrumental_are_not_fake_word_sync() {
    assert_eq!(
        parse("plain", "The quiet\nreturns", 9000)
            .unwrap()
            .precision,
        "plain"
    );
    assert!(
        parse(
            "yaml",
            "version: '1.0'\nmetadata:\n instrumental: true",
            9000
        )
        .unwrap()
        .instrumental
    );
}
#[test]
fn invalid_and_oversize_inputs_fail() {
    for (kind, text) in [
        ("lrc", "[00:99]bad"),
        ("lrc", "[00:NaN]bad"),
        ("yaml", "version: '2.0'"),
        ("yaml", "version: '1.0'\nlines: &x [*x]"),
        ("ttml", "<tt>"),
        ("plain", ""),
    ] {
        assert!(parse(kind, text, 9000).is_err(), "{kind}");
    }
    assert!(parse("plain", &"a".repeat(MAX_TEXT + 1), 9000).is_err());
    assert!(parse("plain", &"line\n".repeat(3001), 9000).is_err());
    assert!(
        parse_json(br#"{"format":"plain","text":"hello","duration_ms":9000,"path":"/etc/passwd"}"#)
            .is_err()
    );
}
#[test]
fn malformed_word_ranges_do_not_hide_missing_text() {
    for word in [
        "{text: wrong, start_ms: 0, end_ms: 1}",
        "{text: test, start_ms: 5, end_ms: 2}",
        "{text: test, start_ms: 0, end_ms: 99999}",
    ] {
        let yaml = format!(
            "version: '1.0'\nlines:\n - text: test\n   start_ms: 0\n   end_ms: 10\n   words: [{word}]"
        );
        assert!(parse("yaml", &yaml, 10000).is_err());
    }
}

#[test]
fn formatted_ttml_and_yaml_reference_limits() {
    let d = parse("ttml", "<tt><body><p begin=\"1s\" end=\"5s\">\n  <span begin=\"0s\" end=\"1s\">Stay</span>\n  <span begin=\"1s\" end=\"4s\">here</span>\n</p></body></tt>", 9000).unwrap();
    assert_eq!(d.precision, "word");
    assert_eq!(d.lines[0].text, "Stay here");
    for text in [
        "version: '1.0'\nlines: [&x {text: a}, *x]",
        "version: '1.0'\nplain: &x hello\nlines: [*x]",
    ] {
        assert!(parse("yaml", text, 9000).is_err());
    }
    assert_eq!(
        parse(
            "yaml",
            "version: '1.0'\nplain: 'Sing * softly & slowly!'
",
            9000
        )
        .unwrap()
        .lines[0]
            .text,
        "Sing * softly & slowly!"
    );
}
