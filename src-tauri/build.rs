fn main() {
    #[cfg(target_os = "macos")]
    build_macos_services();
    tauri_build::build()
}

/// §18: build the `openapp-macos` Swift sidecar (SwiftPM, release) when its sources change and tell debug
/// builds where the binary is (`OPENAPP_MACOS_SERVICES_BIN`; release builds use the copy inside the app
/// bundle, see `src/bundle.rs`). Output stays in `<target>/macos-services`, where `tauri.conf.json`
/// `bundle.macOS.files` picks it up.
/// A failed Swift build is a warning: the kernel then reports host services as unavailable.
#[cfg(target_os = "macos")]
fn build_macos_services() {
    use std::path::PathBuf;
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let pkg = manifest.join("macos-services");
    println!("cargo:rerun-if-changed=macos-services/Package.swift");
    println!("cargo:rerun-if-changed=macos-services/Sources");
    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rerun-if-env-changed=OPENAPP_SKIP_SWIFT");
    // <target>/<profile>/build/<pkg>-<hash>/out → <target>/macos-services
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target = out.ancestors().nth(4).map(PathBuf::from).unwrap_or_else(|| manifest.join("target"));
    let scratch = target.join("macos-services");
    let bin = scratch.join("release").join("openapp-macos");
    if std::env::var("OPENAPP_SKIP_SWIFT").is_ok_and(|v| v == "1") {
        println!("cargo:warning=OPENAPP_SKIP_SWIFT=1: openapp-macos not rebuilt");
    } else {
        let st = std::process::Command::new("swift")
            .args(["build", "-c", "release", "--package-path"])
            .arg(&pkg)
            .arg("--scratch-path")
            .arg(&scratch)
            .env_remove("SDKROOT")
            .status();
        let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
        match st {
            Ok(s) if s.success() => {}
            // a release build is bundled (tauri.conf.json bundle.macOS.files): never ship a stale or missing sidecar
            Ok(s) if release => panic!("openapp-macos (Swift) release build failed ({s}); set OPENAPP_SKIP_SWIFT=1 to skip it"),
            Err(e) if release => panic!("swift not found ({e}); the release bundle needs openapp-macos (OPENAPP_SKIP_SWIFT=1 skips it)"),
            Ok(s) => println!("cargo:warning=openapp-macos (Swift) build failed ({s}); host services will be unavailable"),
            Err(e) => println!("cargo:warning=swift not found ({e}); host services will be unavailable"),
        }
    }
    // the VM runtime the release bundle copies into Contents/MacOS/msb + Contents/Frameworks (tauri.conf.json)
    let msb = manifest.join("vm/runtime");
    for rel in ["bin/msb", "lib/libkrunfw.5.dylib"] {
        if !msb.join(rel).is_file() {
            println!("cargo:warning=VM runtime piece missing: src-tauri/vm/runtime/{rel} (microsandbox 0.7.8; run `node scripts/fetch-runtime.ts`); the app bundle needs it");
        }
    }
    println!("cargo:rustc-env=OPENAPP_MACOS_SERVICES_BIN={}", bin.display());
    // §21 G4: the same binary inside a minimal app bundle, for UserNotifications (needs a bundle identity)
    let helper = bundle_notify_helper(&scratch, &bin, &manifest);
    println!("cargo:rustc-env=OPENAPP_NOTIFY_HELPER_BIN={}", helper.display());
    // Info.plist (usage descriptions) is embedded into the dev binary by tauri-codegen (__TEXT,__info_plist).
}

/// `<target>/macos-services/OpenAppNotify.app` (LSUIElement, ad-hoc signed) around `openapp-macos`, so the
/// notification methods run with a bundle identity. Rebuilt only when the binary changes.
#[cfg(target_os = "macos")]
fn bundle_notify_helper(scratch: &std::path::Path, bin: &std::path::Path, manifest: &std::path::Path) -> std::path::PathBuf {
    let app = scratch.join("OpenAppNotify.app");
    let exe = app.join("Contents").join("MacOS").join("openapp-macos");
    let same = match (std::fs::read(bin), std::fs::read(&exe)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same || !bin.is_file() {
        return exe;
    }
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.polygonface.openapp.notify</string>
  <key>CFBundleName</key><string>OpenApp</string>
  <key>CFBundleDisplayName</key><string>OpenApp</string>
  <key>CFBundleExecutable</key><string>openapp-macos</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
</dict></plist>
"#;
    let _ = std::fs::create_dir_all(exe.parent().unwrap());
    let _ = std::fs::create_dir_all(app.join("Contents").join("Resources"));
    let _ = std::fs::write(app.join("Contents").join("Info.plist"), plist);
    let _ = std::fs::copy(manifest.join("icons").join("icon.icns"), app.join("Contents").join("Resources").join("icon.icns"));
    if let Err(e) = std::fs::copy(bin, &exe) {
        println!("cargo:warning=notification helper bundle: {e}");
        return exe;
    }
    let st = std::process::Command::new("codesign").args(["--force", "--sign", "-"]).arg(&app).output();
    if !st.is_ok_and(|o| o.status.success()) {
        println!("cargo:warning=notification helper: ad-hoc codesign failed; notifications may be refused");
    }
    exe
}
