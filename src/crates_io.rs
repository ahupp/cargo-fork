use anyhow::{anyhow, bail, Context, Result};
use async_compression::tokio::bufread::GzipDecoder;
use futures_util::TryStreamExt;
use reqwest::Url;
use semver::Version;
use serde::Deserialize;
use std::{fs, path::PathBuf, time::Duration};
use tempfile::TempDir;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt},
};
use tokio_tar::Archive;
use tokio_util::io::StreamReader;

/// Fetch the crate archive for the given version from crates.io
async fn fetch_crate_archive(
    crate_name: &str,
    version: &Version,
) -> Result<Archive<impl AsyncRead>> {
    let package_url = format!(
        "https://static.crates.io/crates/{}/{}-{}.crate",
        crate_name, crate_name, version
    );
    let response = reqwest::get(&package_url).await?;

    if response.status() != reqwest::StatusCode::OK {
        bail!("Failed to fetch {}: {}", package_url, response.status());
    }

    // Issue #1: this should be an AsyncRead
    let stream = response
        .bytes_stream()
        .map_err(|err: reqwest::Error| std::io::Error::new(std::io::ErrorKind::Other, err));

    // Issue #2: Cannot use "futures-io", have to use "tokio" for this adapter.
    let buf = StreamReader::new(stream);
    let dec = GzipDecoder::new(buf);

    /*
            Issue #3:

       ^^^^^^ method cannot be called on `Archive<async_compression::tokio::bufread::ZlibDecoder<StreamReader<&mut futures_util::stream::MapErr<impl Stream<Item = Result<bytes::bytes::Bytes, reqwest::Error>>, [closure@src/main.rs:41:54: 44:6]>, bytes::bytes::Bytes>>>` due to unsatisfied trait bounds
       |
      ::: /home/adam/.cargo/registry/src/github.com-1ecc6299db9ec823/async-compression-0.3.14/src/tokio/bufread/mod.rs:10:1
       |
    10 | algos!(tokio::bufread<R>);
       | ------------------------- doesn't satisfy `_: futures_util::AsyncRead`

        Solution: use tokio-tar instead of async-tar

        */

    Ok(Archive::new(dec))
}

/// The git repo, hash and subpath that produced a specific version
/// of a crate uploaded to crates.io
#[derive(Clone)]
pub(crate) struct VcsInfo {
    pub hash: String,
    pub path_in_vcs: PathBuf,
}

pub(crate) async fn lookup_vcs_for_version(
    crate_name: &str,
    version: &Version,
) -> Result<Option<VcsInfo>> {
    let mut archive = fetch_crate_archive(crate_name, version).await?;

    // Issue #4: should this be an AsyncIterator?
    let mut entries = archive.entries()?;
    while let Some(ref mut entry) = entries.try_next().await? {
        let path = entry.path()?;
        let filename = path.file_name().context("Unexpected path")?;
        if filename == ".cargo_vcs_info.json" {
            let tmp = tempfile::NamedTempFile::new()?;
            entry.unpack(tmp.path()).await?;
            let mut contents = String::new();
            let mut r = File::open(tmp).await?;
            r.read_to_string(&mut contents).await?;

            #[derive(Deserialize, Debug)]
            struct GitInfo {
                sha1: Option<String>,
            }

            #[derive(Deserialize, Debug)]
            struct VcsInfoSerde {
                git: Option<GitInfo>,
                path_in_vcs: Option<String>,
            }

            let vcs: VcsInfoSerde = serde_json::from_str(&contents)?;
            let git = vcs
                .git
                .ok_or_else(|| anyhow!("No git info found for {}", crate_name))?;
            let sha1 = git
                .sha1
                .ok_or_else(|| anyhow!("No revision info found for {}", crate_name))?;

            let path_in_vcs = vcs
                .path_in_vcs
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./"));
            return Ok(Some(VcsInfo {
                hash: sha1,
                path_in_vcs,
            }));
        }
    }

    Ok(None)
}

/// Lookup the git repo for the given crate
pub(crate) async fn crate_get_repo(package_name: &str) -> anyhow::Result<Url> {
    let user_agent = format!(
        "cargo-fork/{} (https://github.com/ahupp/cargo-fork)",
        clap::crate_version!()
    );

    let crates_io_client =
        crates_io_api::AsyncClient::new(&user_agent, Duration::from_millis(1000))?;

    let crate_info = crates_io_client.get_crate(package_name).await?;
    let repo_str = crate_info
        .crate_data
        .repository
        .ok_or_else(|| anyhow!("Package {} does not specify a repository", package_name))?;

    Ok(Url::parse(&repo_str)?)
}

pub(crate) async fn unpack_crate_archive(
    tmpdir: &TempDir,
    package_name: &str,
    package_version: &semver::Version,
) -> Result<PathBuf> {
    let mut archive = fetch_crate_archive(package_name, package_version).await?;

    archive.unpack(&tmpdir).await?;

    // The archive is expected to have a single top-level directory like "package-1.2.3"
    // Ensure this is true, and return that directory name
    let mut entries = fs::read_dir(&tmpdir)?;
    let root = entries.nth(0);
    if let Some(entry) = root {
        if entries.count() != 0 {
            bail!("more than one entry in crate archive")
        }
        let entry = entry?;
        Ok(PathBuf::from(entry.file_name()))
    } else {
        bail!("empty crate archive")
    }
}
