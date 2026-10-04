//! Bundle the executable Cargo produced, even when a stale default path exists.

use super::test_dir;
use std::path::Path;
use std::process::{Command, Output};

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn app_bundles_follow_cargo_artifacts_in_environment_and_configured_directories() {
    for configured in [false, true] {
        let root = test_dir(if configured {
            "bundle-config"
        } else {
            "bundle-env"
        });
        for dir in [
            "scripts",
            "src",
            "server/src",
            "assets/licenses",
            "bin",
            "target/debug",
            "cargo-home",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(
            source.join("scripts/build-app.sh"),
            root.join("scripts/build-app.sh"),
        )
        .unwrap();
        std::fs::copy(
            source.join("rust-toolchain.toml"),
            root.join("rust-toolchain.toml"),
        )
        .unwrap();
        std::fs::write(root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"server\"]\n[package]\nname = \"clew\"\nversion = \"0.1.15\"\nedition = \"2024\"\n").unwrap();
        std::fs::write(
            root.join("server/Cargo.toml"),
            "[package]\nname = \"clew-server\"\nversion = \"0.1.15\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"fresh GUI\"); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("server/src/main.rs"),
            "fn main() { println!(\"fresh server\"); }\n",
        )
        .unwrap();
        // Unrelated packaging inputs and macOS signing are isolated stubs;
        // both Rust binaries are built by real Cargo, then really copied/run.
        for name in [
            "clew.icns",
            "NERDFONT-LICENSE.md",
            "licenses/license",
            "Info.plist.template",
        ] {
            std::fs::write(root.join("assets").join(name), "fixture").unwrap();
        }
        executable(
            &root.join("scripts/third-party-notices.sh"),
            "#!/bin/sh\nprintf fixture > \"$1\"\n",
        );
        executable(&root.join("bin/codesign"), "#!/bin/sh\nexit 0\n");
        executable(
            &root.join("target/debug/clew"),
            "#!/bin/sh\necho stale GUI\n",
        );
        executable(
            &root.join("target/debug/clew-server"),
            "#!/bin/sh\necho stale server\n",
        );
        let mut lock = Command::new("cargo");
        lock.current_dir(&root)
            .env("CARGO_HOME", root.join("cargo-home"))
            .args(["generate-lockfile", "--offline"]);
        checked(&mut lock);

        let mut build = Command::new("/bin/bash");
        build
            .current_dir(&root)
            .args(["scripts/build-app.sh", "--debug"])
            .env("CARGO_HOME", root.join("cargo-home"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR");
        if configured {
            std::fs::create_dir_all(root.join(".cargo")).unwrap();
            std::fs::write(
                root.join(".cargo/config.toml"),
                "[build]\ntarget-dir = \"configured outputs\"\n",
            )
            .unwrap();
        } else {
            // Cargo's JSON escapes the quote; shell word splitting must not
            // corrupt it or the spaces when the executable is copied.
            build.env("CARGO_TARGET_DIR", root.join("custom \"outputs\""));
        }
        checked(&mut build);
        for (binary, expected) in [("clew", "fresh GUI\n"), ("clew-server", "fresh server\n")] {
            let output = checked(&mut Command::new(
                root.join("dist/Clew.app/Contents/MacOS").join(binary),
            ));
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
        assert!(
            std::fs::read_to_string(root.join("target/debug/clew"))
                .unwrap()
                .contains("stale")
        );
    }
}
