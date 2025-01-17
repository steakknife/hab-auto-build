use anyhow::{anyhow, Context, Result};
use askalono::Store;
use flate2::bufread::GzDecoder;
use serde_json::Value;
use std::{
    env,
    io::BufReader,
    path::{Path, PathBuf},
};
use tar::Archive;
use tokio::io::AsyncWriteExt;

const SPDX_LICENSE_ARCHIVE: &str =
    "https://github.com/spdx/license-list-data/archive/refs/tags/v3.19.tar.gz";
const LICENSE_ROOT_DIR: &str = "license-list-data-3.19";

async fn download_license_archive(
    out_dir: impl AsRef<Path>,
    license_archive: impl AsRef<Path>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let mut response = client.get(SPDX_LICENSE_ARCHIVE).send().await?;
    let tmp_license_archive = out_dir.as_ref().join("license-archive.tar.gz.part");
    _ = tokio::fs::remove_file(tmp_license_archive.as_path()).await;
    let mut file = tokio::fs::File::create(tmp_license_archive.as_path()).await?;
    while let Some(chunk) = response.chunk().await.with_context(|| {
        anyhow!(
            "Failed to download license file from {}",
            SPDX_LICENSE_ARCHIVE
        )
    })? {
        file.write_all(&chunk).await?;
    }
    file.shutdown().await?;
    tokio::fs::rename(tmp_license_archive.as_path(), license_archive.as_ref()).await?;
    Ok(())
}

fn read_license_archive(
    license_archive: impl AsRef<Path>,
    store: &mut Store,
    deprecated_store: &mut Store,
) -> Result<()> {
    let license_archive = std::fs::File::open(license_archive.as_ref())?;
    let reader = BufReader::new(license_archive);
    let decoder = GzDecoder::new(reader);
    let mut tar = Archive::new(decoder);
    let mut entries = tar.entries().context("Failed to read archive entries")?;

    let json_details_dir = [LICENSE_ROOT_DIR, "json", "details"]
        .iter()
        .collect::<PathBuf>();
    let json_exceptions_dir = [LICENSE_ROOT_DIR, "json", "exceptions"]
        .iter()
        .collect::<PathBuf>();

    while let Some(Ok(entry)) = entries.next() {
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path()?.to_path_buf();
        if entry_path.starts_with(json_details_dir.as_path())
            || entry_path.starts_with(json_exceptions_dir.as_path())
        {
            let reader = std::io::BufReader::new(entry);
            let data: Value = serde_json::from_reader(reader).with_context(|| {
                anyhow!(
                    "Failed to deserialize data from file '{}' into json",
                    entry_path.display()
                )
            })?;
            let is_deprecated = data["isDeprecatedLicenseId"]
                .as_bool()
                .expect("missing license deprecation");

            let id = data["licenseId"]
                .as_str()
                .or_else(|| data["licenseExceptionId"].as_str())
                .expect("missing license id");
            let text = data["licenseText"]
                .as_str()
                .or_else(|| data["licenseExceptionText"].as_str())
                .expect("missing license text");

            if is_deprecated {
                deprecated_store.add_license(id.into(), text.into());
            } else {
                store.add_license(id.into(), text.into());
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let license_archive = Path::new(&out_dir).join("license-archive.tar.gz");
    let license_cache = Path::new(&out_dir).join("license-cache.bin.gz");
    let deprecated_license_cache = Path::new(&out_dir).join("deprecated-license-cache.bin.gz");
    if !license_archive.exists() {
        let mut download_attempts = 3;
        while download_attempts > 0 {
            match download_license_archive(&out_dir, &license_archive).await {
                Ok(_) => {
                    break;
                }
                Err(_) if download_attempts > 0 => {
                    download_attempts -= 1;
                }
                Err(err) => return Err(err),
            }
        }
        _ = tokio::fs::remove_file(license_cache.as_path()).await;
    }

    if !license_cache.is_file() {
        let mut store = Store::new();
        let mut deprecated_store = Store::new();
        read_license_archive(&license_archive, &mut store, &mut deprecated_store)?;
        let mut cache = std::fs::File::create(license_cache)?;
        let mut deprecated_cache = std::fs::File::create(deprecated_license_cache)?;
        store.to_cache(&mut cache)?;
        cache.sync_all()?;
        deprecated_store.to_cache(&mut deprecated_cache)?;
        deprecated_cache.sync_all()?
    }

    Ok(())
}
