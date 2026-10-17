//! `cargo xtask bundle`: a release build packaged for installation.
//!
//! On macOS: `Argus.app` (icon, `Info.plist`, ad-hoc signature) plus a
//! `.dmg` with an Applications link and a `.zip`, all under `target/bundle/`.
//! On Linux (best effort): a `.tar.gz` with the binary, a `.desktop` entry
//! and the icon.
//!
//! Signing, macOS:
//!
//! - by default ad hoc (`codesign --sign -`): enough for the app to launch on
//!   the machine that built it and, after the Gatekeeper prompt, on others;
//! - with `--sign <identity>` (or `ARGUS_SIGN_IDENTITY`), a Developer ID
//!   Application certificate from the keychain, with the hardened runtime
//!   and a secure timestamp, as notarization requires. The `.dmg` is signed
//!   too;
//! - with `--notarize <profile>` (or `ARGUS_NOTARY_PROFILE`) as well, the
//!   `.dmg` is sent to Apple's notary service with `notarytool` (credentials
//!   from a keychain profile made by `xcrun notarytool store-credentials`),
//!   and the ticket is stapled to the `.dmg` and the app before the `.zip` is
//!   made, so Gatekeeper accepts both offline.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

use crate::{icon, png};

/// Name shown by the Finder, the Dock and the menu bar.
const APP_NAME: &str = "Argus";
/// The binary's name, from `crates/argus`.
const EXECUTABLE: &str = "argus";
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "io.github.chris-kode.argus";
/// The workspace version; `xtask` shares it.
const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(target_os = "macos")]
/// The minimum macOS of Rust's `aarch64-apple-darwin` target.
const MACOS_MINIMUM: &str = "11.0";
#[cfg(target_os = "macos")]
/// `PATH` for the app when the Finder or the Dock opens it (they pass only
/// the system directories), so that Homebrew's `git` and `claude` are found
/// as they are from a terminal.
const APP_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// What `bundle` builds.
pub struct Options {
    /// Output directory.
    pub out: PathBuf,
    /// macOS: build arm64 and x86_64 and merge them with `lipo`.
    pub universal: bool,
    /// macOS: the Developer ID Application identity to sign with (a name
    /// or SHA-1 `security find-identity` lists); `None` signs ad hoc.
    pub sign: Option<String>,
    /// macOS: the `notarytool` keychain profile to notarize with; needs
    /// [`Options::sign`].
    pub notarize: Option<String>,
}

/// Builds the release binary and packages it for the host platform.
pub fn run(root: &Path, options: &Options) -> anyhow::Result<()> {
    fs::create_dir_all(&options.out)
        .with_context(|| format!("failed to create {}", options.out.display()))?;
    #[cfg(target_os = "macos")]
    return macos(root, options);
    #[cfg(target_os = "linux")]
    {
        if options.universal || options.sign.is_some() || options.notarize.is_some() {
            bail!("`--universal`, `--sign` and `--notarize` are for macOS only");
        }
        linux(root, &options.out)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = root;
        bail!("`cargo xtask bundle` supports macOS and Linux only");
    }
}

#[cfg(target_os = "macos")]
fn macos(root: &Path, options: &Options) -> anyhow::Result<()> {
    if options.notarize.is_some() && options.sign.is_none() {
        bail!("`--notarize` needs `--sign <Developer ID Application identity>`");
    }
    let (binary, arch) = if options.universal {
        const TRIPLES: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
        check_targets_installed(&TRIPLES)?;
        let mut slices = Vec::new();
        for triple in TRIPLES {
            slices.push(cargo_build(root, Some(triple))?);
        }
        let merged = root.join("target").join("release-universal");
        fs::create_dir_all(&merged)?;
        let merged = merged.join(EXECUTABLE);
        let mut lipo = Command::new("lipo");
        lipo.arg("-create")
            .arg("-output")
            .arg(&merged)
            .args(&slices);
        run_command(&mut lipo)?;
        (merged, "universal")
    } else {
        (cargo_build(root, None)?, host_arch())
    };

    let app = options.out.join(format!("{APP_NAME}.app"));
    eprintln!("Assembling {}", app.display());
    remove_if_exists(&app)?;
    let contents = app.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos_dir)?;
    fs::create_dir_all(&resources)?;
    fs::copy(&binary, macos_dir.join(EXECUTABLE))
        .with_context(|| format!("failed to copy {}", binary.display()))?;
    fs::write(contents.join("Info.plist"), info_plist())?;
    fs::write(contents.join("PkgInfo"), "APPL????")?;
    write_icns(&options.out, &resources.join(format!("{APP_NAME}.icns")))?;

    match &options.sign {
        Some(identity) => {
            // The hardened runtime and a secure timestamp are what the notary
            // service checks for. Argus needs no entitlements: it does not
            // JIT, load plug-ins or use protected resources.
            run_command(
                Command::new("codesign")
                    .args(["--force", "--options", "runtime", "--timestamp"])
                    .args(["--sign", identity])
                    .arg(&app),
            )?;
        }
        // Ad hoc: seals the bundle (Info.plist and resources included)
        // without a certificate.
        None => run_command(
            Command::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(&app),
        )?,
    }
    run_command(
        Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(&app),
    )?;

    let stem = format!("{APP_NAME}-{VERSION}-macos-{arch}");
    let dmg = options.out.join(format!("{stem}.dmg"));
    let staging = options.out.join("dmg-staging");
    remove_if_exists(&staging)?;
    fs::create_dir_all(&staging)?;
    run_command(
        Command::new("ditto")
            .arg(&app)
            .arg(staging.join(format!("{APP_NAME}.app"))),
    )?;
    std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
        .context("failed to link /Applications into the disk image")?;
    remove_if_exists(&dmg)?;
    run_command(
        Command::new("hdiutil")
            .args(["create", "-quiet", "-volname", APP_NAME, "-fs", "HFS+"])
            .args(["-format", "UDZO", "-ov", "-srcfolder"])
            .arg(&staging)
            .arg(&dmg),
    )?;
    fs::remove_dir_all(&staging)?;
    if let Some(identity) = &options.sign {
        run_command(
            Command::new("codesign")
                .args(["--force", "--timestamp", "--sign", identity])
                .arg(&dmg),
        )?;
    }

    if let Some(profile) = &options.notarize {
        notarize(&dmg, profile)?;
        // The ticket covers the app inside the disk image too; stapling it
        // to the app as well lets the `.zip` open offline.
        for path in [&dmg, &app] {
            run_command(Command::new("xcrun").args(["stapler", "staple"]).arg(path))?;
        }
        run_command(
            Command::new("spctl")
                .args(["--assess", "--type", "execute", "--verbose=2"])
                .arg(&app),
        )?;
    }

    // Made last, from the stapled app when notarized.
    let zip = options.out.join(format!("{stem}.zip"));
    remove_if_exists(&zip)?;
    run_command(
        Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(&app)
            .arg(&zip),
    )?;

    eprintln!("\nBuilt:");
    for path in [&app, &dmg, &zip] {
        eprintln!("  {}", path.display());
    }
    Ok(())
}

/// Sends `dmg` to the notary service and waits for its verdict.
#[cfg(target_os = "macos")]
fn notarize(dmg: &Path, profile: &str) -> anyhow::Result<()> {
    eprintln!(
        "Notarizing {} (this usually takes a few minutes)",
        dmg.display()
    );
    let mut submit = Command::new("xcrun");
    submit
        .args([
            "notarytool",
            "submit",
            "--wait",
            "--keychain-profile",
            profile,
        ])
        .args(["--output-format", "json"])
        .arg(dmg);
    eprintln!("$ xcrun notarytool submit --wait --keychain-profile {profile} ...");
    let output = submit
        .output()
        .context("failed to start `xcrun notarytool`")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `--wait` can exit successfully with a rejected submission; the status
    // is in the JSON.
    let compact: String = stdout.split_whitespace().collect();
    let accepted = compact.contains(r#""status":"Accepted""#);
    if !output.status.success() || !accepted {
        bail!(
            "notarization failed:\n{}{}\nSee why with:\n  xcrun notarytool log <submission id> \
             --keychain-profile {profile}",
            stdout.trim(),
            stderr.trim()
        );
    }
    eprintln!("Notarization accepted.");
    Ok(())
}

#[cfg(target_os = "macos")]
/// `Info.plist` of `Argus.app`.
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleExecutable</key>
	<string>{EXECUTABLE}</string>
	<key>CFBundleIconFile</key>
	<string>{APP_NAME}</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>{APP_NAME}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{VERSION}</string>
	<key>CFBundleVersion</key>
	<string>{VERSION}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSEnvironment</key>
	<dict>
		<key>PATH</key>
		<string>{APP_PATH}</string>
	</dict>
	<key>LSMinimumSystemVersion</key>
	<string>{MACOS_MINIMUM}</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Licensed under the Apache License 2.0.</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
/// Renders the icon at every size of a macOS iconset and packs it with
/// `iconutil`.
fn write_icns(out: &Path, icns: &Path) -> anyhow::Result<()> {
    let iconset = out.join(format!("{APP_NAME}.iconset"));
    remove_if_exists(&iconset)?;
    fs::create_dir_all(&iconset)?;
    for size in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let pixels = size * scale;
            let name = match scale {
                1 => format!("icon_{size}x{size}.png"),
                _ => format!("icon_{size}x{size}@{scale}x.png"),
            };
            fs::write(
                iconset.join(name),
                png::encode(pixels, pixels, &icon::render(pixels)),
            )?;
        }
    }
    run_command(
        Command::new("iconutil")
            .args(["--convert", "icns", "--output"])
            .arg(icns)
            .arg(&iconset),
    )?;
    fs::remove_dir_all(&iconset)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux(root: &Path, out: &Path) -> anyhow::Result<()> {
    let binary = cargo_build(root, None)?;
    let stem = format!("{EXECUTABLE}-{VERSION}-linux-{}", host_arch());
    let dir = out.join(&stem);
    eprintln!("Assembling {}", dir.display());
    remove_if_exists(&dir)?;
    fs::create_dir_all(&dir)?;
    fs::copy(&binary, dir.join(EXECUTABLE))
        .with_context(|| format!("failed to copy {}", binary.display()))?;
    fs::write(
        dir.join(format!("{EXECUTABLE}.png")),
        png::encode(256, 256, &icon::render(256)),
    )?;
    fs::write(
        dir.join(format!("{EXECUTABLE}.desktop")),
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={APP_NAME}\n\
             Comment=Review diffs and let Claude Code apply your comments\n\
             Exec={EXECUTABLE}\n\
             Icon={EXECUTABLE}\n\
             Terminal=false\n\
             Categories=Development;\n"
        ),
    )?;
    let archive = out.join(format!("{stem}.tar.gz"));
    run_command(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(out)
            .arg(&stem),
    )?;
    eprintln!("\nBuilt:\n  {}\n  {}", dir.display(), archive.display());
    Ok(())
}

/// `cargo build --release -p argus`, for `triple` when given; returns the
/// binary's path.
fn cargo_build(root: &Path, triple: Option<&str>) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args(["build", "--release", "--locked", "--package", EXECUTABLE]);
    let mut dir = root.join("target");
    if let Some(triple) = triple {
        command.args(["--target", triple]);
        dir.push(triple);
    }
    run_command(&mut command)?;
    Ok(dir.join("release").join(EXECUTABLE))
}

#[cfg(target_os = "macos")]
/// Fails early, with the fix, when a `--universal` slice has no standard
/// library installed.
fn check_targets_installed(triples: &[&str]) -> anyhow::Result<()> {
    let Ok(output) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    else {
        // Not managed by rustup: let cargo report it.
        return Ok(());
    };
    let installed = String::from_utf8_lossy(&output.stdout);
    let missing: Vec<&str> = triples
        .iter()
        .copied()
        .filter(|triple| !installed.lines().any(|line| line.trim() == *triple))
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing Rust targets for a universal build; install them with:\n  rustup target add {}",
            missing.join(" ")
        );
    }
    Ok(())
}

/// The architecture in artifact names: `arm64` like Apple, else Rust's.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" if cfg!(target_os = "macos") => "arm64",
        arch => arch,
    }
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    };
    result.with_context(|| format!("failed to remove {}", path.display()))
}

/// Runs `command`, echoing it, and fails unless it exits successfully.
fn run_command(command: &mut Command) -> anyhow::Result<()> {
    let line = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("$ {line}");
    let status = command
        .status()
        .with_context(|| format!("failed to start `{line}`"))?;
    if !status.success() {
        bail!("`{line}` failed ({status})");
    }
    Ok(())
}
