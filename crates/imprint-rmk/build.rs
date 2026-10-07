use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=keyboard.toml");
    link_script_setup();
    version_embedding();
}

fn link_script_setup() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg=--nmagic");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rustc-link-arg=-Tdefmt.x");
}

fn version_embedding() {
    println!("cargo:rerun-if-env-changed=IMPRINT_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=IMPRINT_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=MOERGO_CONFIG_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=MOERGO_CONFIG_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=IMPRINT_CONFIG_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=IMPRINT_CONFIG_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=MOERGO_RMK_GIT_VERSION");
    println!("cargo:rerun-if-env-changed=IMPRINT_RMK_GIT_VERSION");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let hash = env::var("IMPRINT_GIT_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git(&["rev-parse", "--short=8", "HEAD"]))
        .map(|mut value| {
            value.truncate(8);
            while value.len() < 8 {
                value.push('0');
            }
            value
        })
        .unwrap_or_else(|| "unknown0".to_owned());
    let dirty = match env::var("IMPRINT_GIT_DIRTY").as_deref() {
        Ok("1" | "true") => true,
        Ok("0" | "false") => false,
        Ok(value) => panic!("IMPRINT_GIT_DIRTY must be true/false or 1/0, got {value}"),
        Err(_) => {
            hash != "unknown0"
                && git(&["status", "--porcelain"]).is_some_and(|value| !value.is_empty())
        }
    };

    println!("cargo:rustc-env=MOERGO_REPO_GIT_HASH={hash}");
    println!("cargo:rustc-env=MOERGO_REPO_GIT_DIRTY={}", dirty as u8);

    let config_commit = env::var("MOERGO_CONFIG_GIT_COMMIT")
        .or_else(|_| env::var("IMPRINT_CONFIG_GIT_COMMIT"))
        .unwrap_or_default();
    let config_hash = if config_commit.is_empty() {
        "standalone".to_owned()
    } else {
        assert!(
            config_commit.len() >= 8 && config_commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "MOERGO_CONFIG_GIT_COMMIT must contain at least eight hexadecimal characters"
        );
        config_commit[..8].to_ascii_lowercase()
    };
    let config_dirty = match env::var("MOERGO_CONFIG_GIT_DIRTY")
        .or_else(|_| env::var("IMPRINT_CONFIG_GIT_DIRTY"))
        .as_deref()
    {
        Ok("1" | "true") => true,
        Ok("0" | "false") | Err(_) => false,
        Ok(value) => panic!("MOERGO_CONFIG_GIT_DIRTY must be true/false or 1/0, got {value}"),
    };
    println!("cargo:rustc-env=MOERGO_CONFIG_GIT_HASH={config_hash}");
    println!(
        "cargo:rustc-env=MOERGO_CONFIG_GIT_DIRTY={}",
        config_dirty as u8
    );

    let rmk_version = env::var("MOERGO_RMK_GIT_VERSION")
        .or_else(|_| env::var("IMPRINT_RMK_GIT_VERSION"))
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            git(&[
                "-C",
                "../../dependencies/rmk",
                "describe",
                "--tags",
                "--always",
                "--dirty",
            ])
        })
        .unwrap_or_else(|| "unknown".to_owned());
    assert!(
        rmk_version.len() <= 48 && rmk_version.bytes().all(|byte| byte.is_ascii_graphic()),
        "MOERGO_RMK_GIT_VERSION must be 1-48 printable ASCII characters"
    );
    println!("cargo:rustc-env=MOERGO_RMK_GIT_VERSION={rmk_version}");

    // Avoid a stale dirty bit after incremental builds in a worktree.
    println!("cargo:rerun-if-changed=../../.git");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
