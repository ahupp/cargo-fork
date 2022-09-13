use std::{time::Duration, path::Path};
use anyhow::{Result, bail, anyhow};
use async_compression::tokio::bufread::{GzipDecoder};
use cargo_edit::{LocalManifest, Manifest};
use cargo_lock::{Lockfile, Version};
use clap::{Parser, ArgAction};
use git2::Repository;
use serde::Deserialize;
use tokio::{fs::File, io::AsyncReadExt};
use tokio_tar::Archive;
use tokio_util::io::StreamReader;
use futures_util::TryStreamExt;
use toml_edit::{InlineTable, Value, Item};

static USER_AGENT: &str = "TODO";

#[derive(Deserialize, Debug)]
struct GitInfo {
    sha1: Option<String>,
}

#[derive(Deserialize, Debug)]
struct VcsInfo {
    git: Option<GitInfo>,
    // TODO: handle subpath
    path_in_vcs: Option<String>,
}

async fn lookup_vcs_revision(crate_name: &str, version: &Version) -> Result<String> {

    let package_url = format!("https://static.crates.io/crates/{}/{}-{}.crate", crate_name, crate_name, version);
    let response = reqwest::get(&package_url).await?;

    if response.status() != reqwest::StatusCode::OK {
        bail!("Failed to download {}: {}", package_url, response.status());
    }

    let mut stream = response.bytes_stream().map_err(|err: reqwest::Error| {
        std::io::Error::new(std::io::ErrorKind::Other, err)
     //   anyhow::anyhow!("Failed to read response: {}", err)
    });

    // Issue #2: Cannot use "futures-io", have to use "tokio" for this adapter.
    let buf = StreamReader::new(&mut stream);
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

    let mut archive = Archive::new(dec);
    let mut entries = archive.entries()?;
    while let Some(ref mut entry) = entries.try_next().await? {
        let path = entry.path()?.into_owned();

        if path.file_name().unwrap() == ".cargo_vcs_info.json" {
            let tmp = tempfile::NamedTempFile::new()?;
            entry.unpack(tmp.path()).await?;
            let mut contents = String::new();
            let mut r = File::open(tmp).await?;
            r.read_to_string(&mut contents).await?;
            let vcs : VcsInfo = serde_json::from_str(&contents)?;

            let git = vcs.git.ok_or_else(|| {
                anyhow!("No git info found for {}", crate_name)
            })?;
            let sha1 = git.sha1.ok_or_else(|| {
                anyhow!("No revision info found for {}", crate_name)
            })?;

            return Ok(sha1.clone());
        }
    }

    bail!("No .cargo_vcs_info.json found in {}", crate_name);
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(long, default_value_t=false, action=ArgAction::SetTrue)]
    exact: bool,

    #[clap(long, value_parser)]
    dest_dir: Option<String>,

    #[clap(value_parser)]
    package_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {

    let args = Args::parse();

    let client = crates_io_api::AsyncClient::new(
        USER_AGENT,
        Duration::from_millis(1000))?;

    let crate_info = client.get_crate(&args.package_name).await?;

    let repo = match crate_info.crate_data.repository {
        Some(ref repo) => repo.clone(),
        None => bail!("No repository found for {}", args.package_name),
    };

    let mut manifest = LocalManifest::find(None)?;
    let manifest_dir = manifest.path.parent().unwrap();

    let rev = if args.exact {
        let mut lock_path = manifest.path.clone();
        assert!(lock_path.set_extension("lock"));

        let lock_file = Lockfile::load(&lock_path)?;

        let lock_entry = lock_file.packages.iter()
            .find(|package: &&cargo_lock::Package| package.name.as_str() == args.package_name)
            .ok_or_else(|| {
                anyhow!("No package named {} found", args.package_name)
            })?;

        lookup_vcs_revision(&args.package_name, &lock_entry.version).await?
    } else {
        "HEAD".to_owned()
    };

    let checkout_dir = manifest_dir.join(format!("fork-{}", &args.package_name));
    if checkout_dir.exists() {
        println!("Checkout {} exists, skipping", &checkout_dir.display());
    } else {
        println!("Checkout {} rev:{} into: {}", &repo, &rev, &checkout_dir.display());
        let clone = Repository::clone(&repo, &checkout_dir)?;
        let rev = clone.revparse_single(&rev)?;
        clone.checkout_tree(&rev, None)?;
    }

    // Update patch table
    {
        let doc = &mut manifest.manifest.data;
        let patch_table = doc.entry("patch.crates-io")
            .or_insert_with(|| toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| anyhow!("Failed to add or fetch patch table, {} is malformed", manifest.path.display()))?;

        let mut dep = InlineTable::new();
        dep.insert("path", Value::from(checkout_dir.to_str().unwrap()));

        patch_table[&args.package_name] = Item::Value(Value::InlineTable(dep));
    }

    manifest.write()?;

    Ok(())
}
