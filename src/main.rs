use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tar::Archive;

#[derive(Parser)]
#[command(name = "pkgforge", about = "Source-based Linux package manager in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Download, compile, and install a package by name from a catalog, a TOML manifest, or a direct .tar.gz URL
    Install {
        /// Package name (from catalog), path to a package.toml manifest, or a direct https://...tar.gz URL
        source: String,

        /// Path to a catalog TOML containing [[packages]] entries
        #[arg(long, default_value = "packages.toml")]
        catalog: String,

        /// Build system when passing a direct URL: auto, cmake, autotools, cargo, make, custom
        #[arg(long, default_value = "auto")]
        build_system: String,

        /// Installation prefix (e.g. /usr/local or ~/.local)
        #[arg(long, default_value = "/usr/local")]
        prefix: String,

        /// Keep temporary build directory for debugging
        #[arg(long)]
        keep_work: bool,
    },

    /// List packages available in a catalog TOML file
    List {
        /// Path to a catalog TOML containing [[packages]] entries
        #[arg(long, default_value = "packages.toml")]
        catalog: String,
    },

    /// Remove an installed package
    Remove {
        /// Package name to remove
        name: String,

        /// Installation prefix to check if package is not in the install database
        #[arg(long, default_value = "/usr/local")]
        prefix: String,

        /// Explicit comma-separated file paths to remove
        #[arg(long, value_delimiter = ',')]
        files: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    packages: Vec<PackageManifest>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackageManifest {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    url: String,
    #[serde(default = "default_strip")]
    strip_components: usize,
    #[serde(default = "default_build_system")]
    build_system: String,
    #[serde(default)]
    configure_args: Vec<String>,
    #[serde(default)]
    make_args: Vec<String>,
    #[serde(default = "default_install_args")]
    install_args: Vec<String>,
    #[serde(default)]
    custom_build: Vec<String>,
    #[serde(default)]
    custom_install: Vec<String>,
}

fn default_version() -> String {
    "latest".to_string()
}
fn default_strip() -> usize {
    1
}
fn default_build_system() -> String {
    "auto".to_string()
}
fn default_install_args() -> Vec<String> {
    vec!["install".to_string()]
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledRecord {
    version: String,
    prefix: String,
    files: Vec<String>,
}

type InstallDb = BTreeMap<String, InstalledRecord>;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Install {
            source,
            catalog,
            build_system,
            prefix,
            keep_work,
        } => {
            let manifest = load_manifest(&source, &catalog, &build_system, &prefix)?;
            run_install(&manifest, &prefix, keep_work)?;
        }
        Commands::List { catalog } => {
            list_catalog(&catalog)?;
        }
        Commands::Remove {
            name,
            prefix,
            files,
        } => {
            run_remove(&name, &prefix, &files)?;
        }
    }

    Ok(())
}

fn list_catalog(catalog_path: &str) -> Result<()> {
    let content = fs::read_to_string(catalog_path)
        .with_context(|| format!("Failed to read catalog file: {catalog_path}"))?;
    let cat: CatalogFile =
        toml::from_str(&content).context("Failed to parse catalog TOML")?;

    for pkg in &cat.packages {
        println!("{:<20} {:<15} {}", pkg.name, pkg.version, pkg.build_system);
    }
    println!("Total packages: {}", cat.packages.len());
    Ok(())
}

fn load_manifest(
    source: &str,
    catalog_path: &str,
    build_system: &str,
    prefix: &str,
) -> Result<PackageManifest> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let filename = source
            .split('/')
            .next_back()
            .filter(|s| !s.is_empty())
            .unwrap_or("package");
        let name = filename
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".tgz")
            .to_string();

        let configure_args = if build_system == "cmake" {
            vec![
                "-B".to_string(),
                "build".to_string(),
                format!("-DCMAKE_INSTALL_PREFIX={prefix}"),
            ]
        } else {
            vec![format!("--prefix={prefix}")]
        };

        return Ok(PackageManifest {
            name,
            version: "unknown".to_string(),
            url: source.to_string(),
            strip_components: 1,
            build_system: build_system.to_string(),
            configure_args,
            make_args: vec![],
            install_args: vec!["install".to_string()],
            custom_build: vec![],
            custom_install: vec![],
        });
    }

    let path = Path::new(source);
    if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read manifest file: {source}"))?;
        let manifest: PackageManifest =
            toml::from_str(&content).context("Failed to parse TOML manifest")?;
        return Ok(manifest);
    }

    // Otherwise look up package name inside the catalog TOML
    if Path::new(catalog_path).exists() {
        let content = fs::read_to_string(catalog_path)
            .with_context(|| format!("Failed to read catalog file: {catalog_path}"))?;
        let cat: CatalogFile =
            toml::from_str(&content).context("Failed to parse catalog TOML")?;
        if let Some(found) = cat.packages.into_iter().find(|p| p.name == source) {
            return Ok(found);
        }
    }

    bail!("Package '{}' not found as a manifest file or in catalog '{}'", source, catalog_path);
}

fn run_install(manifest: &PackageManifest, prefix: &str, keep_work: bool) -> Result<()> {
    let work_dir = std::env::temp_dir().join(format!(
        "pkgforge-{}-{}",
        manifest.name,
        std::process::id()
    ));
    let archive_path = work_dir.join("source.tar.gz");
    let src_dir = work_dir.join("src");

    fs::create_dir_all(&src_dir)?;
    println!(
        "==> [1/4] Downloading {} ({})...",
        manifest.name, manifest.version
    );
    download_file(&manifest.url, &archive_path)?;

    println!("==> [2/4] Extracting archive...");
    extract_tar_gz(&archive_path, &src_dir, manifest.strip_components)?;

    let detected_system = if manifest.build_system == "auto" {
        detect_build_system(&src_dir)
    } else {
        manifest.build_system.clone()
    };

    println!("==> [3/4] Compiling using '{}'...", detected_system);
    compile_package(&src_dir, &detected_system, manifest)?;

    println!("==> [4/4] Installing to {}...", prefix);
    install_package(&src_dir, &detected_system, manifest, prefix)?;

    record_installed_files(&manifest.name, &manifest.version, prefix, &src_dir)?;

    if !keep_work {
        let _ = fs::remove_dir_all(&work_dir);
    } else {
        println!("Kept temporary directory at: {}", work_dir.display());
    }

    println!("==> Done! Installed {}.", manifest.name);
    Ok(())
}

fn download_file(url: &str, dest: &Path) -> Result<()> {
    let response = reqwest::blocking::get(url)
        .with_context(|| format!("Failed to request {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error while downloading {url}"))?;

    let bytes = response.bytes().context("Failed to read response body")?;
    fs::write(dest, &bytes)
        .with_context(|| format!("Failed to write archive to {}", dest.display()))?;
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, dest_dir: &Path, strip_components: usize) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!(
                "Refusing archive entry with '..' path component: {}",
                path.display()
            );
        }

        let stripped: PathBuf = path.components().skip(strip_components).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }

        let target = dest_dir.join(stripped);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }

    Ok(())
}

fn detect_build_system(src_dir: &Path) -> String {
    if src_dir.join("Cargo.toml").exists() {
        "cargo".to_string()
    } else if src_dir.join("CMakeLists.txt").exists() {
        "cmake".to_string()
    } else if src_dir.join("configure").exists() {
        "autotools".to_string()
    } else {
        "make".to_string()
    }
}

fn compile_package(src_dir: &Path, system: &str, manifest: &PackageManifest) -> Result<()> {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "2".to_string());

    match system {
        "cargo" => {
            run_cmd("cargo", &["build", "--release"], src_dir)?;
        }
        "cmake" => {
            let mut args = vec![
                "-S".to_string(),
                ".".to_string(),
                "-B".to_string(),
                "build".to_string(),
            ];
            for arg in &manifest.configure_args {
                if arg != "-B" && arg != "build" {
                    args.push(arg.clone());
                }
            }
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            run_cmd("cmake", &arg_refs, src_dir)?;

            run_cmd("cmake", &["--build", "build", "--parallel", &jobs], src_dir)?;
        }
        "autotools" => {
            let cfg_refs: Vec<&str> = manifest.configure_args.iter().map(String::as_str).collect();
            run_cmd("./configure", &cfg_refs, src_dir)?;

            let mut make_args = vec!["-j", &jobs];
            for arg in &manifest.make_args {
                make_args.push(arg);
            }
            run_cmd("make", &make_args, src_dir)?;
        }
        "make" => {
            let mut make_args = vec!["-j", &jobs];
            for arg in &manifest.make_args {
                make_args.push(arg);
            }
            run_cmd("make", &make_args, src_dir)?;
        }
        "custom" => {
            for cmd_str in &manifest.custom_build {
                run_shell(cmd_str, src_dir)?;
            }
        }
        other => bail!("Unsupported build system: {other}"),
    }

    Ok(())
}

fn install_package(
    src_dir: &Path,
    system: &str,
    manifest: &PackageManifest,
    prefix: &str,
) -> Result<()> {
    match system {
        "cargo" => {
            let bin_dir = PathBuf::from(prefix).join("bin");
            fs::create_dir_all(&bin_dir)?;
            run_cmd(
                "cargo",
                &["install", "--path", ".", "--root", prefix],
                src_dir,
            )?;
        }
        "cmake" => {
            run_cmd("cmake", &["--install", "build", "--prefix", prefix], src_dir)?;
        }
        "autotools" | "make" => {
            let mut args: Vec<&str> = manifest.install_args.iter().map(String::as_str).collect();
            let prefix_override = format!("PREFIX={prefix}");
            if system == "make" && !args.iter().any(|a| a.starts_with("PREFIX=")) {
                args.push(&prefix_override);
            }
            run_cmd_maybe_sudo("make", &args, src_dir)?;
        }
        "custom" => {
            for cmd_str in &manifest.custom_install {
                run_shell(cmd_str, src_dir)?;
            }
        }
        other => bail!("Unsupported build system: {other}"),
    }

    Ok(())
}

fn db_path() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("Could not determine LOCALAPPDATA directory")?
    } else {
        let home = std::env::var_os("HOME").context("HOME environment variable not set")?;
        PathBuf::from(home).join(".local").join("share")
    };
    Ok(base.join("pkgforge").join("installed.json"))
}

fn load_db() -> Result<InstallDb> {
    let path = db_path()?;
    if !path.exists() {
        return Ok(InstallDb::new());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read install DB at {}", path.display()))?;
    let db: InstallDb = serde_json::from_str(&content).unwrap_or_default();
    Ok(db)
}

fn save_db(db: &InstallDb) -> Result<()> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(db)?;
    fs::write(&path, json)?;
    Ok(())
}

fn record_installed_files(name: &str, version: &str, prefix: &str, src_dir: &Path) -> Result<()> {
    let mut db = load_db()?;
    let mut candidate_files = Vec::new();

    let bin_candidate = PathBuf::from(prefix).join("bin").join(name);
    if bin_candidate.exists() {
        candidate_files.push(bin_candidate.to_string_lossy().to_string());
    }

    let cmake_manifest = src_dir.join("build").join("install_manifest.txt");
    if cmake_manifest.exists() {
        if let Ok(contents) = fs::read_to_string(&cmake_manifest) {
            for line in contents.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    candidate_files.push(trimmed.to_string());
                }
            }
        }
    }

    candidate_files.sort();
    candidate_files.dedup();

    db.insert(
        name.to_string(),
        InstalledRecord {
            version: version.to_string(),
            prefix: prefix.to_string(),
            files: candidate_files,
        },
    );
    save_db(&db)?;
    Ok(())
}

fn run_remove(name: &str, prefix: &str, explicit_files: &[String]) -> Result<()> {
    let mut db = load_db()?;
    let mut targets: Vec<PathBuf> = explicit_files.iter().map(PathBuf::from).collect();

    if let Some(record) = db.get(name) {
        println!(
            "Found '{}' (version {}, prefix {}) in install database.",
            name, record.version, record.prefix
        );
        for f in &record.files {
            targets.push(PathBuf::from(f));
        }
    } else {
        let fallback_bin = PathBuf::from(prefix).join("bin").join(name);
        println!(
            "'{}' not in install database; checking {}",
            name,
            fallback_bin.display()
        );
        targets.push(fallback_bin);
    }

    let mut removed_any = false;
    for path in targets {
        if path.exists() || path.symlink_metadata().is_ok() {
            println!("  Removing {}", path.display());
            if path.is_dir()
                && !path
                    .symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
            {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("Failed to remove directory {}", path.display()))?;
            } else if let Err(err) = fs::remove_file(&path) {
                #[cfg(unix)]
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    let path_str = path.to_string_lossy().to_string();
                    run_cmd("sudo", &["rm", "-f", &path_str], Path::new("."))?;
                    removed_any = true;
                    continue;
                }
                return Err(err).with_context(|| format!("Failed to remove {}", path.display()));
            }
            removed_any = true;
        }
    }

    if db.remove(name).is_some() {
        save_db(&db)?;
    }

    if removed_any {
        println!("==> Removed {}.", name);
    } else {
        println!(
            "==> No installed files found for '{}'; cleaned database entry if present.",
            name
        );
    }

    Ok(())
}

fn run_cmd(program: &str, args: &[&str], cwd: &Path) -> Result<()> {
    println!("  $ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to execute '{program}'"))?;

    if !status.success() {
        bail!("Command '{program}' exited with status: {status}");
    }
    Ok(())
}

fn run_cmd_maybe_sudo(program: &str, args: &[&str], cwd: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root || run_cmd(program, args, cwd).is_ok() {
            return Ok(());
        }

        println!("  Permission denied; retrying with sudo...");
        let mut sudo_args = vec![program];
        sudo_args.extend_from_slice(args);
        run_cmd("sudo", &sudo_args, cwd)
    }

    #[cfg(not(unix))]
    {
        run_cmd(program, args, cwd)
    }
}

fn run_shell(cmd_str: &str, cwd: &Path) -> Result<()> {
    run_cmd("sh", &["-c", cmd_str], cwd)
}
