use super::{build_app, run, signing_identity};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const BUNDLE_FILES: [&str; 4] = [
    "Contents/Info.plist",
    "Contents/PkgInfo",
    "Contents/MacOS/Auvra",
    "Contents/Helpers/AuvraPlaybackHost",
];
const ASSET_CATALOG: &str = "Contents/Resources/Assets.car";
const STILL_NOTICE: &str = "Still-MIT.txt";
const NOTICE_DIRECTORY: &str = "Contents/Resources/License Notices";
const OFFLINE_LYRICS_HELPER: &str = "Contents/Helpers/AuvraLyricsHost";

fn path_string(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| "Invalid archive path".into())
}

fn capture(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited with {}", output.status));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| format!("{program} returned invalid text"))
}

fn source_status(root: &Path) -> Result<(String, Vec<u8>), String> {
    let status = capture(
        root,
        "git",
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if status.is_empty() {
        return Ok((status, Vec::new()));
    }
    // Mark new files as intent-to-add in a disposable index. The full Git IDs
    // cover binary files too, without staging sources in the user's index.
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .env("GIT_INDEX_FILE", temporary.path().join("index"))
            .current_dir(root)
            .args(args)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!("Git source inspection failed ({})", output.status));
        }
        Ok(output.stdout)
    };
    git(&["read-tree", "HEAD"])?;
    git(&["add", "--intent-to-add", "--", "."])?;
    let diff = git(&[
        "diff",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--ignore-submodules=none",
        "HEAD",
        "--",
        ".",
    ])?;
    Ok((status, diff))
}

fn developer_id_available(requested: &str, identities: &str) -> bool {
    requested.len() == 40
        && requested.bytes().all(|c| c.is_ascii_hexdigit())
        && identities.lines().any(|line| {
            line.split_once('"').is_some_and(|(prefix, name)| {
                name.starts_with("Developer ID Application:")
                    && prefix
                        .split_whitespace()
                        .last()
                        .is_some_and(|hash| hash.eq_ignore_ascii_case(requested))
            })
        })
}

fn developer_id(root: &Path) -> Result<String, String> {
    let requested = env::var("AUVRA_DISTRIBUTION_IDENTITY").map_err(
        |_| "Set AUVRA_DISTRIBUTION_IDENTITY to a Developer ID Application certificate hash",
    )?;
    let identities = capture(
        root,
        "/usr/bin/security",
        &["find-identity", "-v", "-p", "codesigning"],
    )?;
    if !developer_id_available(&requested, &identities) {
        return Err(
            "The selected Developer ID Application signing identity is not available".into(),
        );
    }
    Ok(requested)
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    let file_type = fs::symlink_metadata(source)
        .map_err(|error| format!("Missing archive input {}: {error}", source.display()))?
        .file_type();
    if !file_type.is_file() {
        return Err(format!(
            "Archive input must be a regular file: {}",
            source.display()
        ));
    }
    fs::create_dir_all(destination.parent().unwrap()).map_err(|error| error.to_string())?;
    fs::copy(source, destination).map_err(|error| error.to_string())?;
    Ok(())
}

fn stage_bundle(root: &Path, compiled: &Path, staged: &Path) -> Result<(), String> {
    for entry in BUNDLE_FILES {
        copy_file(&compiled.join(entry), &staged.join(entry))?;
    }
    let assets = compiled.join(ASSET_CATALOG);
    match fs::symlink_metadata(&assets) {
        Ok(_) => copy_file(&assets, &staged.join(ASSET_CATALOG))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let notices = staged.join(NOTICE_DIRECTORY);
    copy_file(&root.join("LICENSE"), &notices.join("LICENSE"))?;
    copy_file(
        &root.join("LICENSES").join(STILL_NOTICE),
        &notices.join(STILL_NOTICE),
    )
}

fn stage_offline_lyrics(root: &Path, pack: &Path, staged: &Path) -> Result<String, String> {
    let resources = staged.join("Contents/Resources/OfflineLyrics");
    run(
        root,
        "python3",
        &[
            "scripts/prepare-offline-lyrics.py",
            path_string(pack)?,
            path_string(&resources)?,
            "--stage-notices",
            path_string(&staged.join("Contents/Resources/License Notices/Offline Lyrics"))?,
        ],
    )?;
    run(
        root,
        "/usr/bin/xcrun",
        &[
            "swiftc",
            "-O",
            "-swift-version",
            "6",
            "-parse-as-library",
            "-target",
            "arm64-apple-macosx14.0",
            "platforms/macos/AuvraLyricsHost/AuvraLyricsHost.swift",
            "-framework",
            "AppKit",
            "-framework",
            "WebKit",
            "-o",
            path_string(&staged.join(OFFLINE_LYRICS_HELPER))?,
        ],
    )?;
    let result = capture(
        root,
        "/usr/bin/shasum",
        &["-a", "256", path_string(&resources.join("manifest.json"))?],
    )?;
    result
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or_else(|| "Missing offline lyrics manifest checksum".into())
}

fn plist_value(root: &Path, app: &Path, key: &str) -> Result<String, String> {
    capture(
        root,
        "/usr/libexec/PlistBuddy",
        &[
            "-c",
            &format!("Print :{key}"),
            path_string(&app.join("Contents/Info.plist"))?,
        ],
    )
}

fn version_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
}

pub fn archive(root: &Path, mode: Option<&str>, extra_args: bool) -> Result<(), String> {
    let public_signature = match (mode, extra_args) {
        (Some("local"), false) => false,
        (Some("developer-id"), false) => true,
        _ => return Err("Usage: cargo xtask dist <local|developer-id>".into()),
    };
    if !cfg!(target_os = "macos") {
        return Err("The macOS archive requires Xcode on macOS".into());
    }
    let offline_pack = env::var_os("AUVRA_OFFLINE_LYRICS_PACK").map(PathBuf::from);
    if let Some(pack) = &offline_pack {
        if public_signature {
            return Err("Offline lyrics packs are only supported for local archives".into());
        }
        if !pack.is_absolute() || !fs::symlink_metadata(pack).is_ok_and(|meta| meta.is_dir()) {
            return Err(
                "AUVRA_OFFLINE_LYRICS_PACK must be an absolute path to a prepared directory".into(),
            );
        }
    }
    let identity = if public_signature {
        developer_id(root)?
    } else {
        signing_identity()
    };
    let revision = capture(root, "git", &["rev-parse", "HEAD"])?;
    if revision.len() < 12 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Git returned an invalid source revision".into());
    }
    let initial_status = source_status(root)?;
    let dirty = !initial_status.0.is_empty();
    if public_signature && dirty {
        return Err(
            "Build the Developer ID archive from a clean checkout, including untracked files"
                .into(),
        );
    }
    let compiled = build_app(root, &root.join("build/DistributionDerivedData"))?;
    if revision != capture(root, "git", &["rev-parse", "HEAD"])?
        || initial_status != source_status(root)?
    {
        return Err("The workspace changed during packaging. Run the archive command again".into());
    }
    let version = plist_value(root, &compiled, "CFBundleShortVersionString")?;
    let build = plist_value(root, &compiled, "CFBundleVersion")?;
    let minimum = plist_value(root, &compiled, "LSMinimumSystemVersion")?;
    if ![&version, &build, &minimum]
        .into_iter()
        .all(|value| version_component(value))
        || plist_value(root, &compiled, "CFBundleIdentifier")? != "app.auvra.player"
        || plist_value(root, &compiled, "CFBundleExecutable")? != "Auvra"
    {
        return Err("Unexpected Auvra bundle metadata".into());
    }
    let label = if public_signature {
        "notary-input"
    } else if offline_pack.is_some() {
        "local-offline-lyrics"
    } else {
        "local"
    };
    let suffix = if dirty { "-dirty" } else { "" };
    let stem = format!(
        "Auvra-{version}-{build}-macos-arm64-{label}-{}{suffix}",
        &revision[..12]
    );
    let directory = root.join("build/distribution");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let destination = directory.join(&stem);
    if destination.exists() {
        return Err(format!(
            "An archive already exists at {}",
            destination.display()
        ));
    }
    let stage = tempfile::Builder::new()
        .prefix(".archive-")
        .tempdir_in(&directory)
        .map_err(|error| error.to_string())?;
    let staged = stage.path().join("Auvra.app");
    stage_bundle(root, &compiled, &staged)?;
    let offline_manifest = offline_pack
        .as_deref()
        .map(|pack| stage_offline_lyrics(root, pack, &staged))
        .transpose()?;
    run(
        root,
        "python3",
        &[
            path_string(&root.join("scripts/package-licenses.py"))?,
            path_string(&staged.join("Contents/Resources/License Notices/Dependencies"))?,
        ],
    )?;
    let helper = staged.join("Contents/Helpers/AuvraPlaybackHost");
    let lyrics_helper = offline_manifest
        .as_ref()
        .map(|_| staged.join(OFFLINE_LYRICS_HELPER));
    let executable = staged.join("Contents/MacOS/Auvra");
    for executable in [&executable, &helper]
        .into_iter()
        .chain(lyrics_helper.iter())
    {
        if capture(root, "/usr/bin/lipo", &["-archs", path_string(executable)?])? != "arm64" {
            return Err("Auvra and its helpers must be arm64".into());
        }
    }
    let timestamp = if public_signature {
        "--timestamp"
    } else {
        "--timestamp=none"
    };
    for signed in std::iter::once(&helper)
        .chain(lyrics_helper.iter())
        .chain(std::iter::once(&staged))
    {
        run(
            root,
            "/usr/bin/codesign",
            &[
                "--force",
                "--options",
                "runtime",
                timestamp,
                "--sign",
                &identity,
                path_string(signed)?,
            ],
        )?;
    }
    let publish = stage.path().join("verified");
    fs::create_dir(&publish).map_err(|error| error.to_string())?;
    let filename = format!("{stem}.zip");
    let zip = publish.join(&filename);
    run(
        root,
        "/usr/bin/ditto",
        &[
            "-c",
            "-k",
            "--keepParent",
            path_string(&staged)?,
            path_string(&zip)?,
        ],
    )?;
    let extracted = stage.path().join("round-trip");
    run(
        root,
        "/usr/bin/ditto",
        &["-x", "-k", path_string(&zip)?, path_string(&extracted)?],
    )?;
    run(
        root,
        "/usr/bin/codesign",
        &[
            "--verify",
            "--deep",
            "--strict",
            path_string(&extracted.join("Auvra.app"))?,
        ],
    )?;
    if lyrics_helper.is_some() {
        run(
            root,
            "/usr/bin/codesign",
            &[
                "--verify",
                "--strict",
                path_string(&extracted.join("Auvra.app").join(OFFLINE_LYRICS_HELPER))?,
            ],
        )?;
    }
    let hash = capture(root, "/usr/bin/shasum", &["-a", "256", path_string(&zip)?])?;
    let digest = hash
        .split_whitespace()
        .next()
        .ok_or("Missing archive checksum")?;
    fs::write(
        publish.join(format!("{filename}.sha256")),
        format!("{digest}  {filename}\n"),
    )
    .map_err(|error| error.to_string())?;
    let offline_info = offline_manifest.map_or_else(String::new, |digest| {
        format!("Offline lyrics manifest SHA-256: {digest}\n")
    });
    fs::write(publish.join("build-info.txt"), format!(
        "Auvra {version} ({build})\nmacOS {minimum}+ arm64\nGit {revision}\nWorking tree changes: {dirty}\nArchive: {label}\nNotarized: no\n{offline_info}",
    )).map_err(|error| error.to_string())?;
    if revision != capture(root, "git", &["rev-parse", "HEAD"])?
        || initial_status != source_status(root)?
    {
        return Err("The workspace changed during packaging. Run the archive command again".into());
    }
    fs::rename(publish, &destination).map_err(|error| error.to_string())?;
    println!("{}", destination.join(filename).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_identity_must_match_a_valid_developer_id_application() {
        let hash = "a".repeat(40);
        for kind in [
            "Apple Development",
            "Developer ID Installer",
            "Developer ID Application",
        ] {
            assert_eq!(
                developer_id_available(
                    &hash,
                    &format!("1) {} \"{kind}: Original fixture\"", hash.to_uppercase())
                ),
                kind == "Developer ID Application"
            );
        }
        assert!(!developer_id_available(
            "Original fixture",
            "0 valid identities found"
        ));
    }

    #[test]
    fn source_snapshot_tracks_content_of_untracked_files_without_staging_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        capture(root, "git", &["init", "--quiet"]).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        capture(root, "git", &["add", ".gitignore"]).unwrap();
        capture(
            root,
            "git",
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(root.join("build/output"), "ignored").unwrap();
        assert!(source_status(root).unwrap().0.is_empty());

        let untracked = root.join("new file");
        fs::write(&untracked, [0, 1]).unwrap();
        let initial = source_status(root).unwrap();
        assert!(!initial.0.is_empty());
        fs::write(&untracked, [0, 2]).unwrap();
        let changed = source_status(root).unwrap();
        assert_eq!(initial.0, changed.0);
        assert_ne!(initial.1, changed.1);
        assert!(
            capture(root, "git", &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        fs::remove_file(untracked).unwrap();
        assert!(source_status(root).unwrap().0.is_empty());
    }

    #[test]
    fn archive_stages_only_the_built_payload_and_required_notices() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let compiled = root.join("source");
        let staged = root.join("staged");
        for entry in BUNDLE_FILES.into_iter().chain([
            "Contents/Resources/fixture-account-data",
            "Contents/Resources/OfflineLyrics/bergamot.wasm",
            OFFLINE_LYRICS_HELPER,
        ]) {
            let path = compiled.join(entry);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "original fixture").unwrap();
        }
        fs::create_dir(root.join("LICENSES")).unwrap();
        fs::write(root.join("LICENSES").join(STILL_NOTICE), "Still notice").unwrap();
        assert!(stage_bundle(root, &compiled, &staged).is_err());
        fs::write(root.join("LICENSE"), "Auvra notice").unwrap();
        stage_bundle(root, &compiled, &staged).unwrap();
        assert!(
            BUNDLE_FILES
                .iter()
                .all(|entry| staged.join(entry).is_file())
        );
        let notices = staged.join(NOTICE_DIRECTORY);
        assert_eq!(
            fs::read_to_string(notices.join("LICENSE")).unwrap(),
            "Auvra notice"
        );
        assert_eq!(
            fs::read_to_string(notices.join(STILL_NOTICE)).unwrap(),
            "Still notice"
        );
        assert!(
            !staged
                .join("Contents/Resources/fixture-account-data")
                .exists()
        );
        assert!(!staged.join(ASSET_CATALOG).exists());
        assert!(!staged.join("Contents/Resources/OfflineLyrics").exists());
        assert!(!staged.join(OFFLINE_LYRICS_HELPER).exists());
        fs::write(compiled.join(ASSET_CATALOG), "original catalog").unwrap();
        stage_bundle(root, &compiled, &staged).unwrap();
        assert!(staged.join(ASSET_CATALOG).is_file());
        assert!(version_component("0.1.0"));
        assert!(!version_component("../1"));
    }
}
