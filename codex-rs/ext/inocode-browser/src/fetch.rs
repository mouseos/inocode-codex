//! First-use download of CEF (65 B-3): read the Spotify CDN index, take the newest stable
//! `minimal` build of the pinned major version, verify its sha1, and unpack only the runtime files
//! into `INOCODE_HOME/browser/cef/<version>/`. Nothing is fetched at build time of InoCode.
use crate::paths;
use sha1::{Digest, Sha1};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const INDEX_URL: &str = "https://cef-builds.spotifycdn.com/index.json";
const BASE_URL: &str = "https://cef-builds.spotifycdn.com/";
/// Must match the `cef` crate in Cargo.toml (156.4.0 -> CEF 156.0.x).
const VERSION_PREFIX: &str = "156.0.";
const MARKER: &str = ".ok";
/// Locale packs kept (the rest is ~50 MB of languages the UI does not use).
const KEEP_LOCALES: &[&str] = &["en-US.pak", "ja.pak"];

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn platform_key() -> Res<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "windows64",
        ("windows", "aarch64") => "windowsarm64",
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linuxarm64",
        (os, arch) => return Err(format!("unsupported platform {os}/{arch} (not verified)").into()),
    })
}

/// A previously fetched CEF directory, newest first.
pub fn installed() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(paths::cef_root())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.join(MARKER).exists()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(VERSION_PREFIX))
        })
        .collect();
    dirs.sort();
    dirs.pop()
}

pub fn ensure() -> Res<PathBuf> {
    if let Some(d) = installed() {
        return Ok(d);
    }
    let key = platform_key()?;
    eprintln!("inocode-browser: CEF is not installed; reading index");
    let index_bytes = ureq::get(INDEX_URL)
        .call()?
        .into_body()
        .with_config()
        .limit(128 * 1024 * 1024)
        .read_to_vec()?;
    let index: serde_json::Value = serde_json::from_slice(&index_bytes)?;
    let versions = index
        .get(key)
        .and_then(|p| p.get("versions"))
        .and_then(|v| v.as_array())
        .ok_or("index has no entry for this platform")?;
    let (version, file) = versions
        .iter()
        .filter(|v| v["channel"] == "stable")
        .filter_map(|v| {
            let ver = v["cef_version"].as_str()?;
            if !ver.starts_with(VERSION_PREFIX) {
                return None;
            }
            let f = v["files"].as_array()?.iter().find(|f| f["type"] == "minimal")?;
            Some((ver.to_string(), f.clone()))
        })
        .next()
        .ok_or("no stable minimal build of the pinned CEF version in the index")?;
    let name = file["name"].as_str().ok_or("index file has no name")?;
    let want_sha1 = file["sha1"].as_str().ok_or("index file has no sha1")?.to_ascii_lowercase();
    let total = file["size"].as_u64().unwrap_or(0);
    let short = version.split('+').next().unwrap_or(&version).to_string();

    let root = paths::cef_root();
    fs::create_dir_all(&root)?;
    let part = root.join(format!("{name}.part"));
    let url = format!("{BASE_URL}{}", name.replace('+', "%2B"));
    eprintln!("inocode-browser: downloading {name} ({} MB)", total / 1_000_000);
    let got = download(&url, &part, total)?;
    if got != want_sha1 {
        let _ = fs::remove_file(&part);
        return Err(format!("sha1 mismatch: index {want_sha1}, downloaded {got}").into());
    }
    eprintln!("inocode-browser: sha1 verified; unpacking");
    let tmp = root.join(format!(".extract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let r = unpack(&part, &tmp);
    let _ = fs::remove_file(&part);
    if let Err(e) = r {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    fs::write(tmp.join(MARKER), format!("{version}\n{want_sha1}\n"))?;
    let dest = root.join(&short);
    let _ = fs::remove_dir_all(&dest);
    fs::rename(&tmp, &dest)?;
    eprintln!("inocode-browser: CEF {short} ready at {}", dest.display());
    Ok(dest)
}

fn download(url: &str, to: &Path, total: u64) -> Res<String> {
    let mut body = ureq::get(url).call()?.into_body();
    let mut reader = body.with_config().limit(u64::MAX).reader();
    let mut out = fs::File::create(to)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut next_report) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
        if done >= next_report {
            eprintln!("inocode-browser: {} / {} MB", done / 1_000_000, total / 1_000_000);
            next_report = done + 25_000_000;
        }
    }
    out.flush()?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Keep Release/* and Resources/* (flattened; locales stay in `locales/`); drop headers, sources,
/// import libraries and the sandbox bootstrap.
fn unpack(archive: &Path, dest: &Path) -> Res<()> {
    let file = BufReader::new(fs::File::open(archive)?);
    let mut ar = tar::Archive::new(bzip2::read::BzDecoder::new(file));
    for entry in ar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let parts: Vec<String> =
            path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let rel: PathBuf = match parts.as_slice() {
            [_, d, f] if d == "Release" || d == "Resources" => {
                if f.ends_with(".lib") || f.starts_with("bootstrap") {
                    continue;
                }
                PathBuf::from(f)
            }
            [_, d, l, f] if d == "Resources" && l == "locales" => {
                if !KEEP_LOCALES.contains(&f.as_str()) {
                    continue;
                }
                PathBuf::from("locales").join(f)
            }
            _ => continue,
        };
        let target = dest.join(rel);
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}
