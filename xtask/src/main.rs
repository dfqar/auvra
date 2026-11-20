use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
mod dist;
fn run(root: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}
fn headers(root: &Path) -> Result<(), String> {
    for (name, config, file) in [
        ("still-ffi", "cbindgen-still.toml", "still.h"),
        ("player-ffi", "cbindgen-player.toml", "player.h"),
    ] {
        cbindgen::Builder::new()
            .with_crate(root.join("crates").join(name))
            .with_config(cbindgen::Config::from_file(root.join(config)).map_err(|e| e.to_string())?)
            .with_only_target_dependencies(true)
            .generate()
            .map_err(|e| e.to_string())?
            .write_to_file(root.join("include").join(file));
    }
    Ok(())
}
fn rust(root: &Path) -> Result<(), String> {
    run(
        root,
        "cargo",
        &[
            "build",
            "--locked",
            "--release",
            "-p",
            "player-ffi",
            "-p",
            "still-ffi",
        ],
    )?;
    headers(root)
}
fn development_identity(output: &str) -> String {
    let identities: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let (prefix, name) = line.split_once('"')?;
            if !name.starts_with("Apple Development:") {
                return None;
            }
            let hash = prefix.split_whitespace().last()?;
            (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
        })
        .collect();
    if identities.len() == 1 {
        identities[0].to_owned()
    } else {
        "-".to_owned()
    }
}
fn signing_identity() -> String {
    if let Ok(value) = env::var("AUVRA_SIGN_IDENTITY") {
        return value;
    }
    Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "-".to_owned(),
            |output| development_identity(&String::from_utf8_lossy(&output.stdout)),
        )
}
fn build_app(root: &Path, derived_data: &Path) -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("The native shell requires macOS and Xcode".into());
    }
    rust(root)?;
    run(
        root,
        "xcodebuild",
        &[
            "-project",
            "platforms/macos/Auvra.xcodeproj",
            "-scheme",
            "Auvra",
            "-configuration",
            "Release",
            "-derivedDataPath",
            derived_data.to_str().ok_or("Invalid build directory")?,
            "build",
            "CODE_SIGNING_ALLOWED=NO",
        ],
    )?;
    let app = derived_data.join("Build/Products/Release/Auvra.app");
    // CPAL includes the process-tap destructor in its stream enum. Keep that
    // optional 14.2 import weak even when this app only opens ordinary outputs.
    let imports = Command::new("nm")
        .args(["-m", "-u"])
        .arg(app.join("Contents/MacOS/Auvra"))
        .output()
        .map_err(|error| error.to_string())?;
    if !imports.status.success() {
        return Err("Could not inspect optional audio imports".into());
    }
    if String::from_utf8_lossy(&imports.stdout)
        .lines()
        .any(|line| {
            (line.contains("_AudioHardwareCreateProcessTap")
                || line.contains("_AudioHardwareDestroyProcessTap"))
                && !line.contains("weak external")
        })
    {
        return Err("Process-tap imports must be optional for macOS 14.0 compatibility".into());
    }
    Ok(app)
}

fn package(root: &Path) -> Result<(), String> {
    let app = build_app(root, &root.join("build/DerivedData"))?;
    // A stable designated requirement keeps Keychain grants across local rebuilds.
    // Never create a certificate or guess among multiple developer identities.
    let identity = signing_identity();
    run(
        root,
        "codesign",
        &[
            "--force",
            "--deep",
            "--sign",
            &identity,
            app.to_str().ok_or("Invalid app path")?,
        ],
    )?;
    run(
        root,
        "codesign",
        &["--verify", "--deep", "--strict", app.to_str().unwrap()],
    )
}

fn check_rust(root: &Path) -> Result<(), String> {
    run(root, "cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        root,
        "cargo",
        &[
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(root, "cargo", &["test", "--locked", "--workspace"])
}
fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut args = env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("headers") => headers(root),
        Some("rust") => rust(root),
        Some("app" | "package") => package(root),
        Some("dist") => dist::archive(root, args.next().as_deref(), args.next().is_some()),
        Some("check-rust") => check_rust(root),
        Some("check") => check_rust(root).and_then(|()| package(root)),
        _ => {
            eprintln!(
                "Usage: cargo xtask <headers|rust|app|package|check-rust|check|dist local|dist developer-id>"
            );
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::development_identity;
    #[test]
    fn only_one_valid_apple_development_identity_is_selected() {
        let hash = "A".repeat(40);
        assert_eq!(
            development_identity(&format!(
                "  1) {hash} \"Apple Development: Original fixture\""
            )),
            hash
        );
        assert_eq!(development_identity("0 valid identities found"), "-");
        assert_eq!(
            development_identity("1) invalid \"Apple Development: Original fixture\""),
            "-"
        );
    }
    #[test]
    fn ambiguous_and_distribution_certificates_are_not_chosen_implicitly() {
        let hash = "A".repeat(40);
        let identity = format!("1) {hash} \"Apple Development: Original fixture\"\n");
        assert_eq!(development_identity(&(identity.clone() + &identity)), "-");
        assert_eq!(
            development_identity(&format!(
                "1) {hash} \"Developer ID Application: Original fixture\""
            )),
            "-"
        );
    }
}
