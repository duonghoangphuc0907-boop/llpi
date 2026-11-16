use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use serde::Deserialize;
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
    /// Download, compile, and install a package from a TOML manifest or direct .tar.gz URL
    Install {
        /// Path to a package.toml manifest or a direct https://...tar.gz URL
        source: String,

        /// Build system when passing a direct URL: auto, cmake, autotools, cargo
        #[arg(long, default_value = "auto")]
        build_system: String,

        /// Installation prefix (e.g. /usr/local or ~/.local)
        #[arg(long, default_value = "/usr/local")]
        prefix: String,

        /// Keep temporary build directory for debugging
        #[arg(long)]
        keep_work: bool,
    },
}

#[derive(Debug, Deserialize)]
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Install {
            source,
            build_system,
            prefix,
            keep_work,
        } => {
            let manifest = load_manifest(&source, &build_system, &prefix)?;
            run_install(&manifest, &prefix, keep_work)?;
        }
    }

    Ok(())
}

fn load_manifest(source: &str, build_system: &str, prefix: &str) -> Result<PackageManifest> {
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

        Ok(PackageManifest {
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
        })
    } else {
        let content = fs::read_to_string(source)
            .with_context(|| format!("Failed to read manifest file: {source}"))?;
        let manifest: PackageManifest =
            toml::from_str(&content).context("Failed to parse TOML manifest")?;
        Ok(manifest)
    }
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
    fs::write(dest, &bytes).with_context(|| format!("Failed to write archive to {}", dest.display()))?;
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, dest_dir: &Path, strip_components: usize) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // Guard against zip-slip / path traversal
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("Refusing archive entry with '..' path component: {}", path.display());
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

            run_cmd(
                "cmake",
                &["--build", "build", "--parallel", &jobs],
                src_dir,
            )?;
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
            run_cmd(
                "cmake",
                &["--install", "build", "--prefix", prefix],
                src_dir,
            )?;
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
    // If already root or installing into a user-writable directory, run directly
    let is_root = unsafe { libc_geteuid() } == 0;
    if is_root || run_cmd(program, args, cwd).is_ok() {
        return Ok(());
    }

    println!("  Permission denied; retrying with sudo...");
    let mut sudo_args = vec![program];
    sudo_args.extend_from_slice(args);
    run_cmd("sudo", &sudo_args, cwd)
}

fn run_shell(cmd_str: &str, cwd: &Path) -> Result<()> {
    run_cmd("sh", &["-c", cmd_str], cwd)
}

unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}
