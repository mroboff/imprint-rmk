use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use serde_json::json;
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

mod config_profile;

const UF2_MAGIC0: u32 = 0x0a32_4655;
const UF2_MAGIC1: u32 = 0x9e5d_5157;
const UF2_MAGIC_END: u32 = 0x0ab1_6f30;
const UF2_FLAG_FAMILY_ID: u32 = 0x0000_2000;
const UF2_PAYLOAD_SIZE: usize = 256;
const APPLICATION_START: u32 = 0x0002_6000;
const APPLICATION_END: u32 = 0x000d_c000;

/// A board whose release bundle is built from a canonical copy of the
/// tracked tree, so the image does not depend on where the checkout lives.
struct BoardSpec {
    /// The crate under `crates/`, also the bundle's project name.
    crate_name: &'static str,
    /// The short name in the canonical build's paths under /tmp.
    tag: &'static str,
    /// The prefix of the environment variables the crate's build.rs reads.
    env_prefix: &'static str,
    /// Where the bundle lands, under the repository root.
    dist_dir: &'static str,
    halves: [Half; 2],
}

const GO60: BoardSpec = BoardSpec {
    crate_name: "go60-rmk",
    tag: "go60",
    env_prefix: "GO60",
    dist_dir: "dist/go60",
    halves: [
        Half::new("left", "lh", "go60_lh", 0x9809_b007),
        Half::new("right", "rh", "go60_rh", 0x980a_b007),
    ],
};

/// The Cyboard Imprint flashes through the Adafruit nRF52840 bootloader,
/// whose family id is the same for both halves.
const IMPRINT: BoardSpec = BoardSpec {
    crate_name: "imprint-rmk",
    tag: "imprint",
    env_prefix: "IMPRINT",
    dist_dir: "dist/imprint",
    halves: [
        Half::new("left", "lh", "imprint_lh", 0xada5_2840),
        Half::new("right", "rh", "imprint_rh", 0xada5_2840),
    ],
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let root = repo_root()?;
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("check") if args.next().is_none() => check(&root),
        Some("dist") if args.next().is_none() => dist(&root),
        Some("dist-go60") if args.next().is_none() => dist_go60(&root),
        Some("dist-imprint") if args.next().is_none() => dist_imprint(&root),
        Some("verify-config-profile") => {
            let stock = args.next().ok_or("verify-config-profile requires STOCK CONFIGURED")?;
            let configured = args.next().ok_or("verify-config-profile requires STOCK CONFIGURED")?;
            if args.next().is_some() {
                return Err("verify-config-profile accepts exactly two files".into());
            }
            let digests = config_profile::verify(&root.join(stock), &root.join(configured))?;
            println!("configuration sha256: {}", digests.configuration);
            println!("platform profile sha256: {}", digests.platform_profile);
            Ok(())
        }
        Some("verify-stock-config") => {
            let stock = args.next().ok_or("verify-stock-config requires STOCK CONFIGURED")?;
            let configured = args.next().ok_or("verify-stock-config requires STOCK CONFIGURED")?;
            let bilateral_thumbs = match args.next().as_deref() {
                None => false,
                Some("--allow-bilateral-thumbs") if args.next().is_none() => true,
                _ => {
                    return Err(
                        "verify-stock-config accepts STOCK CONFIGURED [--allow-bilateral-thumbs]".into(),
                    );
                }
            };
            let digests = config_profile::verify_stock_defaults(
                &root.join(stock),
                &root.join(configured),
                bilateral_thumbs,
            )?;
            println!("configuration sha256: {}", digests.configuration);
            Ok(())
        }
        Some("inspect-uf2") => {
            let path = args.next().ok_or("inspect-uf2 requires a file")?;
            let expected_family = match args.next().as_deref() {
                None => None,
                Some("--family") => {
                    let value = args.next().ok_or("--family requires a hexadecimal family ID")?;
                    let family = u32::from_str_radix(value.trim_start_matches("0x"), 16)?;
                    if args.next().is_some() {
                        return Err("inspect-uf2 accepts FILE [--family HEX]".into());
                    }
                    Some(family)
                }
                _ => return Err("inspect-uf2 accepts FILE [--family HEX]".into()),
            };
            let info = inspect_uf2(&root.join(path), expected_family)?;
            println!(
                "{}: {} blocks, {}-{}, family {}",
                info.path.display(),
                info.blocks,
                hex(info.start),
                hex(info.end),
                info.family.map(hex).unwrap_or_else(|| "(none)".to_owned())
            );
            Ok(())
        }
        _ => Err("usage: cargo run -p xtask -- <check|dist|dist-go60|verify-config-profile STOCK CONFIGURED|verify-stock-config STOCK CONFIGURED [--allow-bilateral-thumbs]|inspect-uf2 FILE [--family HEX]>".into()),
    }
}

fn repo_root() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("xtask must live at crates/xtask")?
        .canonicalize()?)
}

fn check(root: &Path) -> Result<()> {
    validate_submodule(root, false)?;
    validate_local_paths(root)?;

    run_command(
        root,
        "cargo",
        &["check", "--workspace", "--all-targets"],
        &[],
    )?;
    run_command(root, "cargo", &["test", "--workspace"], &[])?;
    // `moergo-config-wasm` is wasm32-gated, so the host check above compiles an
    // empty library and would pass with the crate thoroughly broken. Anything
    // added to a shared type in `moergo-config` has to reach the browser too;
    // this is the only check that notices when it did not.
    run_command(
        root,
        "cargo",
        &[
            "check",
            "-p",
            "moergo-config-wasm",
            "--target",
            "wasm32-unknown-unknown",
        ],
        &[],
    )?;
    Ok(())
}

fn validate_submodule(root: &Path, allow_dirty: bool) -> Result<String> {
    let line = git(root, &["submodule", "status", "--", "dependencies/rmk"])?;
    // `git()` trims output, including the leading space that `git submodule
    // status` uses for a clean checkout. Dirty/uninitialized/conflicted
    // prefixes survive trimming, so reject those explicitly.
    if matches!(line.as_bytes().first(), Some(b'+' | b'-' | b'U')) {
        return Err(format!(
            "dependencies/rmk is uninitialized, modified, or on the wrong commit: {line}"
        )
        .into());
    }

    let expected = git(root, &["rev-parse", "HEAD:dependencies/rmk"])?;
    let actual = git(root, &["-C", "dependencies/rmk", "rev-parse", "HEAD"])?;
    if actual != expected {
        return Err(format!("RMK checkout {actual} does not match gitlink {expected}").into());
    }
    let status = git(root, &["-C", "dependencies/rmk", "status", "--porcelain"])?;
    if !status.is_empty() && !allow_dirty {
        return Err("dependencies/rmk has local changes".into());
    }
    Ok(actual)
}

fn validate_local_paths(root: &Path) -> Result<()> {
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !ignored_tree(entry, root))
    {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() == OsStr::new("Cargo.toml") {
            validate_manifest_paths(root, entry.path())?;
        }
    }
    Ok(())
}

fn ignored_tree(entry: &DirEntry, root: &Path) -> bool {
    if entry.path() == root {
        return true;
    }
    if !entry.file_type().is_dir() {
        return true;
    }
    matches!(
        entry.file_name().to_str(),
        Some(".git" | ".worktrees" | "target" | "node_modules")
    ) || entry.path() == root.join("dependencies/rmk")
}

fn validate_manifest_paths(root: &Path, manifest: &Path) -> Result<()> {
    let contents = fs::read_to_string(manifest)?;
    let value: toml::Value = toml::from_str(&contents)?;
    let mut paths = Vec::new();
    collect_path_values(&value, &mut paths);
    let manifest_dir = manifest.parent().ok_or("manifest has no parent")?;
    for relative in paths {
        let resolved = normalize(&manifest_dir.join(relative));
        if !resolved.starts_with(root) {
            return Err(format!(
                "{} has a path outside the repository: {relative}",
                manifest.strip_prefix(root).unwrap_or(manifest).display()
            )
            .into());
        }
    }
    Ok(())
}

fn collect_path_values<'a>(value: &'a toml::Value, paths: &mut Vec<&'a str>) {
    match value {
        toml::Value::Table(table) => {
            if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
                paths.push(path);
            }
            for child in table.values() {
                collect_path_values(child, paths);
            }
        }
        toml::Value::Array(array) => {
            for child in array {
                collect_path_values(child, paths);
            }
        }
        _ => {}
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn dist(root: &Path) -> Result<()> {
    let allow_dirty = env::var("MOERGO_ALLOW_DIRTY").as_deref() == Ok("1")
        || env::var("GLOVE80_ALLOW_DIRTY").as_deref() == Ok("1");
    let rmk_commit = validate_submodule(root, allow_dirty)?;
    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    if dirty && !allow_dirty {
        return Err(
            "release bundles require a clean repository (set MOERGO_ALLOW_DIRTY=1 only for local validation)"
                .into(),
        );
    }

    let version = toml_value(
        root.join("crates/glove80-rmk/Cargo.toml"),
        &["package", "version"],
    )?;
    let rust_toolchain = toml_value(root.join("rust-toolchain.toml"), &["toolchain", "channel"])?;
    let source_commit = git(root, &["rev-parse", "HEAD"])?;
    let rmk_version = deterministic_submodule_identity(root, &rmk_commit)?;
    let config_commit = env::var("MOERGO_CONFIG_GIT_COMMIT")
        .or_else(|_| env::var("GLOVE80_CONFIG_GIT_COMMIT"))
        .unwrap_or_else(|_| "standalone".to_owned());
    let config_dirty = env::var("MOERGO_CONFIG_GIT_DIRTY")
        .or_else(|_| env::var("GLOVE80_CONFIG_GIT_DIRTY"))
        .unwrap_or_else(|_| "false".to_owned());

    let firmware_dir = root.join("crates/glove80-rmk");
    let config_path = effective_config_path(&firmware_dir);
    let config_digests = config_profile::digests(&config_path)?;
    let build_hash_seed = firmware_build_hash_seed(&source_commit, &rmk_commit, &config_digests);
    let rustflags = reproducible_rustflags(root, &config_path);
    // `--locked` refuses a stale lockfile instead of silently rewriting it:
    // that rewrite dirties the tree mid-run, and the next bundle then fails
    // the clean-repository check with no hint of the real cause.
    for binary in ["glove80_lh", "glove80_rh"] {
        run_command(
            &firmware_dir,
            "cargo",
            &["build", "--locked", "--release", "--bin", binary],
            &[
                ("MOERGO_RMK_GIT_VERSION", &rmk_version),
                ("RMK_BUILD_HASH_SEED", &build_hash_seed),
                ("RUSTFLAGS", &rustflags),
            ],
        )?;
    }

    let target = firmware_dir.join("target/thumbv7em-none-eabihf/release");
    let dist = output_dir(root, "dist");
    fs::create_dir_all(&dist)?;
    let halves = [
        Half::new("left", "lh", "glove80_lh", 0x9807_b007),
        Half::new("right", "rh", "glove80_rh", 0x9808_b007),
    ];
    for half in &halves {
        let base = format!("glove80-rmk-{version}-{}", half.suffix);
        let elf = dist.join(format!("{base}.elf"));
        fs::copy(target.join(half.binary), &elf)?;
        set_readable_permissions(&elf)?;
        let elf_bytes = fs::read(&elf)?;
        let segments = load_elf_segments(&elf_bytes)?;
        let uf2 = encode_uf2(&segments, half.family)?;
        fs::write(dist.join(format!("{base}.uf2")), uf2)?;
    }

    package_release(
        &dist,
        "glove80-rmk",
        &version,
        &source_commit,
        dirty,
        &config_commit,
        config_dirty == "true",
        &rmk_commit,
        &rmk_version,
        &rust_toolchain,
        &config_digests,
        &halves,
    )
}

fn dist_go60(root: &Path) -> Result<()> {
    dist_board(root, &GO60)
}

fn dist_imprint(root: &Path) -> Result<()> {
    dist_board(root, &IMPRINT)
}

fn dist_board(root: &Path, board: &BoardSpec) -> Result<()> {
    let allow_dirty = env::var("MOERGO_ALLOW_DIRTY").as_deref() == Ok("1")
        || env::var(format!("{}_ALLOW_DIRTY", board.env_prefix)).as_deref() == Ok("1");
    let rmk_commit = validate_submodule(root, allow_dirty)?;
    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    if dirty && !allow_dirty {
        return Err(format!(
            "{} release bundles require a clean repository (set MOERGO_ALLOW_DIRTY=1 only for local validation)",
            board.crate_name
        )
        .into());
    }

    let crate_dir = format!("crates/{}", board.crate_name);
    let version = toml_value(
        root.join(&crate_dir).join("Cargo.toml"),
        &["package", "version"],
    )?;
    let rust_toolchain = toml_value(root.join("rust-toolchain.toml"), &["toolchain", "channel"])?;
    let source_commit = git(root, &["rev-parse", "HEAD"])?;
    let rmk_version = deterministic_submodule_identity(root, &rmk_commit)?;
    let config_commit = env::var("MOERGO_CONFIG_GIT_COMMIT")
        .or_else(|_| env::var(format!("{}_CONFIG_GIT_COMMIT", board.env_prefix)))
        .unwrap_or_else(|_| "standalone".to_owned());
    let config_dirty = env::var("MOERGO_CONFIG_GIT_DIRTY")
        .or_else(|_| env::var(format!("{}_CONFIG_GIT_DIRTY", board.env_prefix)))
        .unwrap_or_else(|_| "false".to_owned());
    let source_dirty = if dirty { "true" } else { "false" };

    let firmware_dir = root.join(&crate_dir);
    let config_path = effective_config_path(&firmware_dir);
    let config_digests = config_profile::digests(&config_path)?;
    let build_hash_seed = firmware_build_hash_seed(&source_commit, &rmk_commit, &config_digests);
    let build = CanonicalBuild::prepare(root, &config_path, board.tag)?;
    let build_firmware_dir = build.root.join(&crate_dir);
    let rustflags = reproducible_rustflags(&build.root, &build.config_path);
    let commit_var = format!("{}_GIT_COMMIT", board.env_prefix);
    let dirty_var = format!("{}_GIT_DIRTY", board.env_prefix);
    // `--locked` refuses a stale lockfile instead of silently rewriting it:
    // that rewrite dirties the tree mid-run, and the next bundle then fails
    // the clean-repository check with no hint of the real cause.
    for half in &board.halves {
        run_command(
            &build_firmware_dir,
            "cargo",
            &["build", "--locked", "--release", "--bin", half.binary],
            &[
                (&commit_var, &source_commit),
                (&dirty_var, source_dirty),
                ("MOERGO_RMK_GIT_VERSION", &rmk_version),
                ("RMK_BUILD_HASH_SEED", &build_hash_seed),
                ("KEYBOARD_TOML_PATH", build.config_path_str()),
                ("CARGO_HOME", &build.cargo_home),
                ("RUSTFLAGS", &rustflags),
            ],
        )?;
    }

    let target = build_firmware_dir.join("target/thumbv7em-none-eabihf/release");
    let dist = output_dir(root, board.dist_dir);
    fs::create_dir_all(&dist)?;
    for half in &board.halves {
        let base = format!("{}-{version}-{}", board.crate_name, half.suffix);
        let elf = dist.join(format!("{base}.elf"));
        fs::copy(target.join(half.binary), &elf)?;
        set_readable_permissions(&elf)?;
        let elf_bytes = fs::read(&elf)?;
        let segments = load_elf_segments(&elf_bytes)?;
        let uf2 = encode_uf2(&segments, half.family)?;
        fs::write(dist.join(format!("{base}.uf2")), uf2)?;
    }

    package_release(
        &dist,
        board.crate_name,
        &version,
        &source_commit,
        dirty,
        &config_commit,
        config_dirty == "true",
        &rmk_commit,
        &rmk_version,
        &rust_toolchain,
        &config_digests,
        &board.halves,
    )
}

fn toml_value(path: PathBuf, keys: &[&str]) -> Result<String> {
    let contents = fs::read_to_string(&path)?;
    let document = toml::from_str::<toml::Value>(&contents)?;
    let mut value = &document;
    for key in keys {
        value = value
            .get(*key)
            .ok_or_else(|| format!("{} has no {}", path.display(), keys.join(".")))?;
    }
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} {} is not a string", path.display(), keys.join(".")).into())
}

fn effective_config_path(firmware_dir: &Path) -> PathBuf {
    env::var_os("KEYBOARD_TOML_PATH")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                firmware_dir.join(path)
            }
        })
        .unwrap_or_else(|| firmware_dir.join("keyboard.toml"))
}

fn output_dir(root: &Path, default: &str) -> PathBuf {
    env::var_os("MOERGO_DIST_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join(default))
}

fn firmware_build_hash_seed(
    source_commit: &str,
    rmk_commit: &str,
    config_digests: &config_profile::Digests,
) -> String {
    format!(
        "moergo-rmk:{source_commit}:rmk:{rmk_commit}:platform:{}",
        config_digests.platform_profile
    )
}

struct CanonicalBuild {
    root: PathBuf,
    config_dir: PathBuf,
    config_path: PathBuf,
    config_path_text: String,
    cargo_home: String,
    lock_path: PathBuf,
}

impl CanonicalBuild {
    fn prepare(root: &Path, config_path: &Path, tag: &str) -> Result<Self> {
        let lock_path = PathBuf::from(format!("/tmp/moergo-rmk-{tag}-build.lock"));
        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|error| {
                format!(
                    "cannot acquire the canonical {tag} build lock {}: {error}",
                    lock_path.display()
                )
            })?;
        writeln!(lock, "{}", std::process::id())?;

        let config_dir = format!("/tmp/moergo-rmk-{tag}-config");
        let build = Self {
            root: PathBuf::from(format!("/tmp/moergo-rmk-{tag}-source")),
            config_dir: PathBuf::from(&config_dir),
            config_path: PathBuf::from(&config_dir).join("keyboard.toml"),
            config_path_text: format!("{config_dir}/keyboard.toml"),
            cargo_home: format!("/tmp/moergo-rmk-{tag}-cargo"),
            lock_path,
        };
        build.reset()?;
        copy_tracked_tree(root, &build.root)?;
        fs::create_dir_all(&build.config_dir)?;
        fs::copy(config_path, &build.config_path)?;
        fs::create_dir_all(&build.cargo_home)?;
        Ok(build)
    }

    fn config_path_str(&self) -> &str {
        &self.config_path_text
    }

    fn reset(&self) -> Result<()> {
        for path in [&self.root, &self.config_dir] {
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
        Ok(())
    }
}

impl Drop for CanonicalBuild {
    fn drop(&mut self) {
        let _ = self.reset();
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn copy_tracked_tree(root: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let files = command_output(
        root,
        "git",
        &["ls-files", "--recurse-submodules", "-z"],
        &[],
    )?;
    for encoded in files
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = std::str::from_utf8(encoded)?;
        let relative = Path::new(relative);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!(
                "git reported an unsafe tracked path: {}",
                relative.display()
            )
            .into());
        }
        let source = root.join(relative);
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            // Local validation may include tracked files deleted from the worktree.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            copy_symlink(&source, &target)?;
        } else if metadata.is_file() {
            fs::copy(&source, &target)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_dir() {
            // `git ls-files --recurse-submodules` reports the directory entry
            // for an uninitialized nested submodule. Its contents are absent
            // by definition, and Cargo will report a useful error if needed.
            continue;
        } else {
            return Err(format!(
                "tracked path is not a file or symlink: {}",
                source.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    Ok(())
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(fs::read_link(source)?, target)?;
    } else {
        std::os::windows::fs::symlink_file(fs::read_link(source)?, target)?;
    }
    Ok(())
}

fn reproducible_rustflags(root: &Path, config_path: &Path) -> String {
    let mut mappings = vec![(root.to_path_buf(), "/source/moergo-rmk")];
    let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".cargo"))
    });
    if let Some(cargo_home) = cargo_home {
        mappings.push((cargo_home, "/cargo"));
    }
    if !config_path.starts_with(root)
        && let Some(config_dir) = config_path.parent()
    {
        mappings.push((config_dir.to_path_buf(), "/source/config"));
    }

    let inherited = env::var("RUSTFLAGS").unwrap_or_default();
    mappings
        .into_iter()
        .fold(inherited, |mut flags, (from, to)| {
            if !flags.is_empty() {
                flags.push(' ');
            }
            write!(flags, "--remap-path-prefix={}={to}", from.display()).unwrap();
            flags
        })
}

fn deterministic_submodule_identity(root: &Path, commit: &str) -> Result<String> {
    let dirty = !git(root, &["-C", "dependencies/rmk", "status", "--porcelain"])?.is_empty();
    Ok(format!(
        "{}{}",
        commit
            .get(..8)
            .ok_or("RMK commit is shorter than eight characters")?,
        if dirty { "-dirty" } else { "" }
    ))
}

#[cfg(unix)]
fn set_readable_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_readable_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy)]
struct Half {
    name: &'static str,
    suffix: &'static str,
    binary: &'static str,
    family: u32,
}

impl Half {
    const fn new(
        name: &'static str,
        suffix: &'static str,
        binary: &'static str,
        family: u32,
    ) -> Self {
        Self {
            name,
            suffix,
            binary,
            family,
        }
    }
}

struct Segment<'a> {
    address: u32,
    data: &'a [u8],
}

fn load_elf_segments(elf: &[u8]) -> Result<Vec<Segment<'_>>> {
    if read_u32(elf, 0)? != 0x464c_457f {
        return Err("not an ELF file".into());
    }
    if elf.get(4) != Some(&1) || elf.get(5) != Some(&1) {
        return Err("expected ELF32 little-endian".into());
    }
    let header_offset = read_u32(elf, 28)? as usize;
    let entry_size = read_u16(elf, 42)? as usize;
    let entry_count = read_u16(elf, 44)? as usize;
    let mut segments = Vec::new();
    for index in 0..entry_count {
        let offset = header_offset
            .checked_add(index.checked_mul(entry_size).ok_or("ELF header overflow")?)
            .ok_or("ELF header overflow")?;
        let kind = read_u32(elf, offset)?;
        let file_offset = read_u32(elf, offset + 4)? as usize;
        let address = read_u32(elf, offset + 12)?;
        let file_size = read_u32(elf, offset + 16)? as usize;
        if kind == 1 && file_size > 0 {
            let end = file_offset
                .checked_add(file_size)
                .ok_or("ELF segment overflow")?;
            let data = elf
                .get(file_offset..end)
                .ok_or("ELF segment is out of bounds")?;
            segments.push(Segment { address, data });
        }
    }
    segments.sort_by_key(|segment| segment.address);
    if segments.is_empty() {
        return Err("no PT_LOAD segments with file data".into());
    }
    Ok(segments)
}

fn encode_uf2(segments: &[Segment<'_>], family: u32) -> Result<Vec<u8>> {
    let start = segments.first().ok_or("no ELF segments")?.address;
    let end = segments.iter().try_fold(start, |end, segment| {
        let segment_end = segment
            .address
            .checked_add(u32::try_from(segment.data.len())?)
            .ok_or("firmware address overflow")?;
        Ok::<u32, Box<dyn std::error::Error>>(end.max(segment_end))
    })?;
    let image_len = usize::try_from(end - start)?;
    let mut image = vec![0xff; image_len];
    for segment in segments {
        let offset = usize::try_from(segment.address - start)?;
        let end = offset + segment.data.len();
        image
            .get_mut(offset..end)
            .ok_or("ELF segment falls outside flattened image")?
            .copy_from_slice(segment.data);
    }

    let blocks = image.len().div_ceil(UF2_PAYLOAD_SIZE);
    let mut uf2 = vec![0; blocks * 512];
    for block in 0..blocks {
        let output = &mut uf2[block * 512..(block + 1) * 512];
        write_u32(output, 0, UF2_MAGIC0)?;
        write_u32(output, 4, UF2_MAGIC1)?;
        write_u32(output, 8, UF2_FLAG_FAMILY_ID)?;
        write_u32(output, 12, start + u32::try_from(block * UF2_PAYLOAD_SIZE)?)?;
        write_u32(output, 16, UF2_PAYLOAD_SIZE as u32)?;
        write_u32(output, 20, u32::try_from(block)?)?;
        write_u32(output, 24, u32::try_from(blocks)?)?;
        write_u32(output, 28, family)?;
        let chunk_start = block * UF2_PAYLOAD_SIZE;
        let chunk_end = (chunk_start + UF2_PAYLOAD_SIZE).min(image.len());
        output[32..32 + chunk_end - chunk_start].copy_from_slice(&image[chunk_start..chunk_end]);
        write_u32(output, 508, UF2_MAGIC_END)?;
    }
    Ok(uf2)
}

struct Uf2Info {
    path: PathBuf,
    blocks: usize,
    start: u32,
    end: u32,
    family: Option<u32>,
}

fn inspect_uf2(path: &Path, expected_family: Option<u32>) -> Result<Uf2Info> {
    let data = fs::read(path)?;
    if data.is_empty() || data.len() % 512 != 0 {
        return Err(format!("{} is not a non-empty UF2 block stream", path.display()).into());
    }
    let blocks = data.len() / 512;
    let mut start = u32::MAX;
    let mut end = 0;
    let mut family = None;
    for block in 0..blocks {
        let offset = block * 512;
        if read_u32(&data, offset)? != UF2_MAGIC0
            || read_u32(&data, offset + 4)? != UF2_MAGIC1
            || read_u32(&data, offset + 508)? != UF2_MAGIC_END
        {
            return Err(
                format!("{} has invalid UF2 magic in block {block}", path.display()).into(),
            );
        }
        let flags = read_u32(&data, offset + 8)?;
        let address = read_u32(&data, offset + 12)?;
        let payload_size = read_u32(&data, offset + 16)?;
        if payload_size == 0 || payload_size > 476 {
            return Err(format!("{} has an invalid UF2 payload size", path.display()).into());
        }
        if read_u32(&data, offset + 20)? != u32::try_from(block)?
            || read_u32(&data, offset + 24)? != u32::try_from(blocks)?
        {
            return Err(format!(
                "{} has inconsistent UF2 block numbering at block {block}",
                path.display()
            )
            .into());
        }
        if expected_family.is_some() && flags != UF2_FLAG_FAMILY_ID {
            return Err(format!(
                "{} block {block} must be a family-tagged application flash block",
                path.display()
            )
            .into());
        }
        if flags & UF2_FLAG_FAMILY_ID != 0 {
            let current = read_u32(&data, offset + 28)?;
            if family.is_some_and(|previous| previous != current) {
                return Err(format!("{} contains multiple family IDs", path.display()).into());
            }
            family = Some(current);
        }
        start = start.min(address);
        end = end.max(
            address
                .checked_add(payload_size)
                .ok_or("UF2 address overflow")?,
        );
    }
    if let Some(expected) = expected_family
        && family != Some(expected)
    {
        return Err(format!(
            "{} has family {}, expected {}",
            path.display(),
            family.map(hex).unwrap_or_else(|| "(none)".to_owned()),
            hex(expected)
        )
        .into());
    }
    if expected_family.is_some() && (start < APPLICATION_START || end > APPLICATION_END) {
        return Err(format!(
            "{} range {}-{} is outside {}-{}",
            path.display(),
            hex(start),
            hex(end),
            hex(APPLICATION_START),
            hex(APPLICATION_END)
        )
        .into());
    }
    Ok(Uf2Info {
        path: path.to_owned(),
        blocks,
        start,
        end,
        family,
    })
}

#[allow(clippy::too_many_arguments)]
fn package_release(
    dist: &Path,
    project: &str,
    version: &str,
    source_commit: &str,
    dirty: bool,
    config_commit: &str,
    config_dirty: bool,
    rmk_commit: &str,
    rmk_version: &str,
    rust_toolchain: &str,
    config_digests: &config_profile::Digests,
    halves: &[Half],
) -> Result<()> {
    let mut artifacts = Vec::new();
    let mut checksums = String::new();
    for half in halves {
        let base = format!("{project}-{version}-{}", half.suffix);
        let uf2_name = format!("{base}.uf2");
        let elf_name = format!("{base}.elf");
        let uf2_path = dist.join(&uf2_name);
        let elf_path = dist.join(&elf_name);
        let info = inspect_uf2(&uf2_path, Some(half.family))?;
        // `cargo check` never links, so a board can drift to the edge of the
        // application partition without anything saying so. Report the slack
        // on every bundle so the next change sees it coming.
        let free = APPLICATION_END - info.end;
        let uf2_hash = sha256(&uf2_path)?;
        let elf_hash = sha256(&elf_path)?;
        artifacts.push(json!({
            "half": half.name,
            "target": "thumbv7em-none-eabihf",
            "uf2": {
                "file": uf2_name,
                "sha256": uf2_hash,
                "blocks": info.blocks,
                "familyId": hex(half.family),
                "addressStart": hex(info.start),
                "addressEnd": hex(info.end),
            },
            "elf": { "file": elf_name, "sha256": elf_hash },
            "freeBytes": free,
        }));
        writeln!(checksums, "{uf2_hash}  {uf2_name}")?;
        writeln!(checksums, "{elf_hash}  {elf_name}")?;
        println!(
            "{}: {}-{}, {} free, {}, {}",
            half.name,
            hex(info.start),
            hex(info.end),
            free,
            hex(half.family),
            uf2_hash
        );
    }

    let configuration = if config_commit == "standalone" {
        serde_json::Value::Null
    } else {
        json!({ "commit": config_commit, "dirty": config_dirty })
    };
    let manifest = json!({
        "schemaVersion": 2,
        "project": project,
        "version": version,
        "source": { "commit": source_commit, "dirty": dirty },
        "configuration": configuration,
        "rmk": { "commit": rmk_commit, "version": rmk_version },
        "rustToolchain": rust_toolchain,
        "configurationHashes": {
            "schemaVersion": config_profile::SCHEMA_VERSION,
            "canonical": config_digests.configuration,
            "platformProfile": config_digests.platform_profile,
        },
        "applicationRange": { "start": hex(APPLICATION_START), "end": hex(APPLICATION_END) },
        "artifacts": artifacts,
    });
    fs::write(
        dist.join("manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    fs::write(dist.join("SHA256SUMS"), checksums)?;
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let digest = Sha256::digest(fs::read(path)?);
    Ok(format!("{digest:x}"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or("unexpected end of binary data")?
        .try_into()?;
    Ok(u16::from_le_bytes(value))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected end of binary data")?
        .try_into()?;
    Ok(u32::from_le_bytes(value))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or("unexpected end of binary data")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn hex(value: u32) -> String {
    format!("0x{value:x}")
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = command_output(root, "git", args, &[])?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn command_output(
    directory: &Path,
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Result<Output> {
    let mut command = Command::new(program);
    command.current_dir(directory).args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
    let output = command.output()?;
    if !output.status.success() {
        io::stderr().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        return Err(format!("{program} {} failed with {}", args.join(" "), output.status).into());
    }
    Ok(output)
}

fn run_command(
    directory: &Path,
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Result<()> {
    let mut command = Command::new(program);
    command.current_dir(directory).args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{program} {} failed with {status}", args.join(" ")).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uf2_round_trip_preserves_range_and_family() {
        let segment = Segment {
            address: APPLICATION_START,
            data: &[1, 2, 3, 4],
        };
        let bytes = encode_uf2(&[segment], 0x9807_b007).unwrap();
        let path = env::temp_dir().join(format!("glove80-xtask-{}.uf2", std::process::id()));
        fs::write(&path, bytes).unwrap();
        let info = inspect_uf2(&path, Some(0x9807_b007)).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(info.blocks, 1);
        assert_eq!(info.start, APPLICATION_START);
        assert_eq!(info.end, APPLICATION_START + UF2_PAYLOAD_SIZE as u32);
    }

    #[test]
    fn tracked_tree_copy_preserves_edits_and_omits_deleted_files() {
        let root = env::temp_dir().join(format!("moergo-copy-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        run_command(&root, "git", &["init", "--quiet"], &[]).unwrap();
        fs::write(root.join("kept.rs"), "old").unwrap();
        fs::write(root.join("deleted.rs"), "obsolete").unwrap();
        run_command(&root, "git", &["add", "kept.rs", "deleted.rs"], &[]).unwrap();
        fs::write(root.join("kept.rs"), "edited").unwrap();
        fs::remove_file(root.join("deleted.rs")).unwrap();
        fs::write(root.join("untracked.rs"), "untracked").unwrap();
        let output = root.join("snapshot");
        copy_tracked_tree(&root, &output).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("kept.rs")).unwrap(),
            "edited"
        );
        assert!(!output.join("deleted.rs").exists());
        assert!(!output.join("untracked.rs").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_flash_images_before_packaging() {
        let bytes = encode_uf2(
            &[Segment {
                address: APPLICATION_START,
                data: &[1; 300],
            }],
            0x9807_b007,
        )
        .unwrap();
        let path = env::temp_dir().join(format!("moergo-invalid-{}.uf2", std::process::id()));
        fs::write(&path, &bytes).unwrap();
        assert!(inspect_uf2(&path, Some(0x9808_b007)).is_err());
        for (offset, value) in [
            (512 + 8, 0),
            (8, UF2_FLAG_FAMILY_ID | 1),
            (16, 0),
            (16, 477),
            (12, APPLICATION_START - 256),
            (12, APPLICATION_END),
            (512 + 28, 0x9808_b007),
        ] {
            let mut invalid = bytes.clone();
            write_u32(&mut invalid, offset, value).unwrap();
            fs::write(&path, invalid).unwrap();
            assert!(
                inspect_uf2(&path, Some(0x9807_b007)).is_err(),
                "offset {offset}, value {value}"
            );
        }
        fs::remove_file(path).unwrap();
    }

    /// Every key in `[layout].map` carries a hand tag.
    ///
    /// RMK decides bilateral home row mods from these tags, and an untagged
    /// key is `Hand::Unknown`, which never counts as the same hand. Worse, the
    /// code generator falls back to an all-`Unknown` table without complaining
    /// if the tags go missing, so dropping one disables the feature silently
    /// rather than failing the build.
    #[test]
    fn every_mapped_key_declares_a_hand() {
        let root = repo_root().unwrap();
        let text = fs::read_to_string(root.join("crates/glove80-rmk/keyboard.toml")).unwrap();
        let start = text.find("map = \"\"\"").unwrap();
        let body = &text[start + "map = \"\"\"".len()..];
        let map = &body[..body.find("\"\"\"").unwrap()];

        let untagged: Vec<&str> = map
            .split_whitespace()
            .filter(|token| token.starts_with('('))
            .filter(|token| {
                // (row, col[, hand][, @shape]) — the hand is the third field.
                token
                    .trim_matches(['(', ')'])
                    .split(',')
                    .nth(2)
                    .is_none_or(|hand| !matches!(hand, "L" | "R" | "*"))
            })
            .collect();
        assert!(untagged.is_empty(), "untagged keys: {untagged:?}");
    }

    #[test]
    fn sha256_matches_reference_vector() {
        let path = env::temp_dir().join(format!("glove80-sha-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        let hash = sha256(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
