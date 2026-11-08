//! `bulkhead bundle` and `bulkhead keygen`: build signed `.rbp` bundles.
//!
//! An app's `bulkhead.toml` has the app's identity and its `appserver`
//! section:
//!
//! ```toml
//! [app]
//! name = "counter"
//! version = "1.0.0"
//! description = "A stateful counter"
//!
//! [appserver]
//! registered_names = ["counter"]
//!
//! [appserver.upgrade]
//! mode = "snapshot"
//! snapshot_version = 1
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bulkhead_bundle::manifest::{AppSpec, is_valid_name};
use bulkhead_bundle::{NATIVE_PLATFORMS, check_key_permissions, check_program};
use minisign::{KeyPair, PublicKey, SecretKey, SecretKeyBox};
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde::Deserialize;

/// Environment variable holding a signing key's password.
pub const PASSWORD_ENV: &str = "BULKHEAD_KEY_PASSWORD";

/// The `[app]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppTable {
    name: String,
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    repository: Option<String>,
}

/// A `bulkhead.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorManifest {
    app: AppTable,
    #[serde(default)]
    appserver: Option<toml::Value>,
}

/// What `bundle` was asked to do.
#[derive(Debug)]
pub struct BundleRequest<'a> {
    /// The `bulkhead.toml`.
    pub manifest: &'a Path,
    /// The app's program (an executable or a rustbridge `.so`) for this
    /// machine's platform.
    pub program: Option<&'a Path>,
    /// The app's programs by platform, in place of `program`
    /// (`--program linux-aarch64=PATH`).
    pub programs: &'a [(Platform, PathBuf)],
    /// The minisign secret key.
    pub key: &'a Path,
    /// Files to ship read-only at `/priv`.
    pub priv_dir: Option<&'a Path>,
    /// Output path; defaults to `<name>-<version>.rbp`.
    pub out: Option<&'a Path>,
}

/// What `bundle` produced.
#[derive(Debug)]
pub struct Bundled {
    /// The written bundle.
    pub path: PathBuf,
    /// App name and version.
    pub name: String,
    /// Version.
    pub version: String,
    /// The signer's public key.
    pub public_key: String,
    /// The platforms packed.
    pub platforms: Vec<String>,
    /// Files added under `priv/`.
    pub priv_files: usize,
}

/// Read a minisign secret key (refused if its group or others may read
/// it, as ssh does), decrypting it with `BULKHEAD_KEY_PASSWORD`
/// when set (minisign prompts on the terminal otherwise).
fn load_key(path: &Path) -> anyhow::Result<(String, SecretKey)> {
    check_key_permissions(path)?;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read key {}", path.display()))?;
    let parse = || {
        SecretKeyBox::from_string(&text)
            .with_context(|| format!("{} is not a minisign secret key", path.display()))
    };
    // An unencrypted key opens as is; otherwise decrypt it.
    let sk = match parse()?.into_unencrypted_secret_key() {
        Ok(sk) => sk,
        Err(_) => parse()?
            .into_secret_key(std::env::var(PASSWORD_ENV).ok())
            .map_err(|e| anyhow::anyhow!("cannot open key {}: {e}", path.display()))?,
    };
    let pk = PublicKey::from_secret_key(&sk).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((pk.to_base64(), sk))
}

/// Every file under `dir`, as (path relative to `dir` with `/`, bytes),
/// sorted.
fn read_tree(dir: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> anyhow::Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                let relative = path.strip_prefix(root)?;
                let parts: Vec<String> = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push((parts.join("/"), std::fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Parse `--program`'s `PLATFORM=PATH`.
///
/// # Errors
///
/// A message for a value without `=` or a platform `native-process` apps
/// do not run on.
pub fn parse_program(value: &str) -> Result<(Platform, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| format!("'{value}' is not PLATFORM=PATH"))?;
    let platform = Platform::parse(name)
        .filter(|p| NATIVE_PLATFORMS.contains(p))
        .ok_or_else(|| {
            format!("unknown platform '{name}' (native-process apps run on linux-x86_64 and linux-aarch64)")
        })?;
    Ok((platform, PathBuf::from(path)))
}

/// A `native-process` app's programs by platform: `--program`'s, or the
/// one argument as this machine's.
fn native_programs<'a>(req: &BundleRequest<'a>) -> anyhow::Result<Vec<(Platform, &'a Path)>> {
    match (req.program, req.programs) {
        (Some(_), [_, ..]) => bail!(
            "give the program as the argument (for this machine's platform) or with --program, not both"
        ),
        (None, []) => bail!(
            "a native-process app needs its program: as the argument for this machine, or --program PLATFORM=PATH"
        ),
        (Some(program), []) => {
            let platform = Platform::current()
                .filter(|p| NATIVE_PLATFORMS.contains(p))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "native-process apps run on linux-x86_64 and linux-aarch64, not this machine ({}-{}); name the program's platform with --program",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    )
                })?;
            Ok(vec![(platform, program)])
        }
        (None, programs) => {
            let mut out: Vec<(Platform, &Path)> = Vec::new();
            for (platform, path) in programs {
                if out.iter().any(|(p, _)| p == platform) {
                    bail!("--program {} given twice", platform.as_str());
                }
                out.push((*platform, path.as_path()));
            }
            Ok(out)
        }
    }
}

/// Build, sign and verify a bundle.
///
/// # Errors
///
/// A message for an unreadable or invalid manifest (with the field's path),
/// a program that is not an ELF file for its platform, an unreadable key, or a bundle that fails to verify after writing.
pub fn bundle(req: &BundleRequest<'_>) -> anyhow::Result<Bundled> {
    let text = std::fs::read_to_string(req.manifest)
        .with_context(|| format!("cannot read {}", req.manifest.display()))?;
    let author: AuthorManifest =
        toml::from_str(&text).with_context(|| format!("{}", req.manifest.display()))?;
    let app = author.app;
    if !is_valid_name(&app.name) {
        bail!(
            "app name '{}' must be lowercase letters, digits, '-' and '_'",
            app.name
        );
    }
    semver::Version::parse(&app.version).with_context(|| format!("version '{}'", app.version))?;
    let appserver = serde_json::to_value(
        author
            .appserver
            .unwrap_or(toml::Value::Table(toml::Table::new())),
    )?;
    AppSpec::from_json_value(&appserver).map_err(|e| anyhow::anyhow!("[appserver] {e}"))?;

    let read = |path: &Path| {
        std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
    };
    let programs = native_programs(req)?;
    for (platform, path) in &programs {
        check_program(*platform, &read(path)?).with_context(|| format!("{}", path.display()))?;
    }

    let mut manifest = Manifest::new(&app.name, &app.version);
    manifest.plugin.description = app.description;
    manifest.plugin.authors = app.authors;
    manifest.plugin.license = app.license;
    manifest.plugin.repository = app.repository;
    let (public_key, secret_key) = load_key(req.key)?;
    // A release variant per platform: a node runs the one for its own.
    let mut builder = BundleBuilder::new(manifest);
    for (platform, path) in &programs {
        builder = builder.add_library(*platform, path)?;
    }
    let platforms = programs
        .iter()
        .map(|(p, _)| p.as_str().to_string())
        .collect();
    let mut builder = builder.with_extension(bulkhead_bundle::manifest::SECTION, appserver);
    let mut priv_files = 0;
    if let Some(dir) = req.priv_dir {
        for (relative, bytes) in read_tree(dir)? {
            builder = builder
                .add_checksummed_bytes(&format!("{}{relative}", bulkhead_bundle::PRIV_DIR), bytes);
            priv_files += 1;
        }
    }
    let path = req.out.map_or_else(
        || PathBuf::from(format!("{}-{}.rbp", app.name, app.version)),
        Path::to_path_buf,
    );
    builder
        .with_signing_key(public_key.clone(), secret_key)
        .write(&path)?;

    // Read it back exactly as a node on each of its platforms will.
    let keys = std::slice::from_ref(&public_key);
    let opened = programs.iter().try_for_each(|(platform, _)| {
        bulkhead_bundle::VerifiedBundle::open_for_platform(&path, keys, *platform).map(drop)
    });
    opened.with_context(|| format!("{} does not verify", path.display()))?;
    Ok(Bundled {
        path,
        name: app.name,
        version: app.version,
        public_key,
        platforms,
        priv_files,
    })
}

/// Write a new minisign key pair to `<prefix>.key` and `<prefix>.pub`.
/// Encrypted with `BULKHEAD_KEY_PASSWORD` unless `no_password`; without
/// either, minisign prompts for a password. Returns the public key.
///
/// # Errors
///
/// A message if a file exists or cannot be written.
pub fn keygen(prefix: &Path, no_password: bool) -> anyhow::Result<String> {
    let key_path = prefix.with_extension("key");
    let pub_path = prefix.with_extension("pub");
    for p in [&key_path, &pub_path] {
        if p.exists() {
            bail!("{} already exists", p.display());
        }
    }
    let pair = if no_password {
        KeyPair::generate_unencrypted_keypair()
    } else {
        KeyPair::generate_encrypted_keypair(std::env::var(PASSWORD_ENV).ok())
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let sk_box = pair.sk.to_box(None).map_err(|e| anyhow::anyhow!("{e}"))?;
    let pk_box = pair.pk.to_box().map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(&key_path, sk_box.into_string())?;
    std::fs::write(&pub_path, pk_box.into_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(pair.pk.to_base64())
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod bundle_tests;
