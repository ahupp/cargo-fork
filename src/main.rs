/*
TODO
- verify it comes from crates.io
- path_in_vcs


https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html

*/

use std::{time::Duration, path::{PathBuf, Path}, fs, collections::{HashSet, HashMap}, process::Command};
use cargo_metadata::{PackageId, Metadata};
use reqwest::Url;
use semver::Version;
use anyhow::{Result, bail, anyhow, Context};
use async_compression::tokio::bufread::{GzipDecoder};
use cargo_edit::{Manifest};
use git2::{Repository, build::{CheckoutBuilder, RepoBuilder}, FetchOptions, RemoteCallbacks};
use serde::Deserialize;
use tempfile::TempDir;
use tokio::{fs::File, io::{AsyncReadExt, AsyncRead}};
use tokio_tar::Archive;
use tokio_util::io::StreamReader;
use futures_util::TryStreamExt;
use toml_edit::{InlineTable, Value, Item, Table};
use clap::Parser;

#[derive(Clone)]
struct VcsInfo {
    hash: String,
    path_in_repo: PathBuf,
}

async fn fetch_crate_archive(crate_name: &str, version: &Version) -> Result<Archive<impl AsyncRead>> {

    let package_url = format!(
        "https://static.crates.io/crates/{}/{}-{}.crate",
        crate_name, crate_name, version);
    let response = reqwest::get(&package_url).await?;

    if response.status() != reqwest::StatusCode::OK {
        bail!("Failed to fetch {}: {}", package_url, response.status());
    }

    // Issue #1: this should be an AsyncRead
    let stream = response.bytes_stream().map_err(|err: reqwest::Error| {
        std::io::Error::new(std::io::ErrorKind::Other, err)
    });

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

async fn lookup_vcs_revision(crate_name: &str, version: &Version) -> Result<Option<VcsInfo>> {
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

            let vcs : VcsInfoSerde = serde_json::from_str(&contents)?;
            let git = vcs.git.ok_or_else(|| {
                anyhow!("No git info found for {}", crate_name)
            })?;
            let sha1 = git.sha1.ok_or_else(|| {
                anyhow!("No revision info found for {}", crate_name)
            })?;

            let path_in_vcs = vcs.path_in_vcs
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./"));
            return Ok(Some(VcsInfo {hash: sha1, path_in_repo: path_in_vcs}));
        }
    }

    Ok(None)
}


#[derive(clap::ValueEnum, Debug, Clone, Eq, PartialEq)]
enum Source {
    /// Patch with the contents of the crate uploaded to crates.io
    Crate,
    /// Patch with the VCS HEAD revision
    VCSHead,
    /// Patch with the VCS revision corresponding to the current crate version
    VCSCurrent,
}

#[derive(clap::Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[arg(long, value_enum, default_value_t = Source::VCSCurrent)]
    source: Source,

    #[arg(long)]
    dest_dir: Option<PathBuf>,

    #[arg()]
    crate_name: String,
}

async fn crate_get_repo(package_name: &str) -> anyhow::Result<Url> {
    let user_agent = format!("cargo-fork/{} (https://github.com/ahupp/cargo-fork)", clap::crate_version!());

    let crates_io_client = crates_io_api::AsyncClient::new(
        &user_agent,
        Duration::from_millis(1000))?;

    let crate_info = crates_io_client.get_crate(package_name).await?;
    let repo_str = crate_info.crate_data.repository
        .ok_or_else(|| anyhow!("Package {} does not specify a repository", package_name))?;

    Ok(Url::parse(&repo_str)?)
}

fn toml_get_or_create_table_by_path<'a>(path: &[&str], table: &'a mut Table) -> anyhow::Result<&'a mut Table> {
    let mut current = table;
    for key in path {
        current = current.entry(key)
            .or_insert_with(|| toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| anyhow!("Maformed Cargo.toml"))?;
        current.set_implicit(true);

    }
    Ok(current)
}


fn print_changed_deps(metadata_before: &Metadata, metadata_after: &Metadata) {
    /// Given the output of `cargo metadata`, return a map from PackageId to the set of PackageIds it depends on
    fn cargo_meta_to_depmap(metadata: &Metadata) -> HashMap<&PackageId, HashSet<&PackageId>> {
        let resolve = metadata.resolve.as_ref().expect("metadata resolve missing");
        HashMap::from_iter(
            resolve.nodes.iter().map(|node| (
                &node.id,
                node.dependencies.iter().collect()
            )
        ))
    }

    let depmap_after = cargo_meta_to_depmap(&metadata_after);
    let depmap_before = cargo_meta_to_depmap(&metadata_before);

    for (pkg_id, deps_after) in &depmap_after {
        if let Some(deps_before) = depmap_before.get(pkg_id) {
            if deps_before == deps_after {
                continue;
            }
            let del_dep = deps_before.difference(deps_after);
            let add_dep = deps_after.difference(deps_before);
            println!("{}:", pkg_id);
            for dep in del_dep {
                println!("  - {}", dep);
            }
            for dep in add_dep {
                println!("  + {}", dep);
            }
        } else {
            println!("+{}:", pkg_id);
        }
    }
}

fn manifest_insert_patch(manifest: &mut Manifest, package_name: &str, patch_dir: &Path) -> Result<()> {

    let root_table = manifest.data.as_table_mut();

    let patch_table = toml_get_or_create_table_by_path(&["patch", "crates-io"], root_table)?;

    patch_table[&package_name] = {
        let mut dep = InlineTable::new();
        dep.insert("path", Value::from(patch_dir.to_str().unwrap()));
        // Explicitly set package name to handle cases where we patch multiple versions
        // see https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#using-patch-with-multiple-versions
        // TODO: rename key to support multiple patched versions
        dep.insert("package", Value::from(package_name.clone()));

        Item::Value(Value::InlineTable(dep))
    };

    Ok(())
}

fn query_metadata(cdir: &Path) -> Result<Metadata> {
    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.current_dir(cdir);
    Ok(metadata_cmd.exec()?)
}

fn git_checkout(repo_url: &Url, vcs_revision: &str, checkout_dir: &Path) -> Result<()> {
    let repo = if checkout_dir.try_exists()? {
        // TODO: make sure it fails if repo is dirty
        Repository::open(&checkout_dir)
    } else {
        println!("git clone: {} into {}", repo_url, checkout_dir.display());
        let mut cb = RemoteCallbacks::new();
        cb.transfer_progress(|_progress| {
            // TODO: show progress
            //println!("{}: {}", progress.received_objects(), progress.total_objects());
            true
        });
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(cb);
        RepoBuilder::new().fetch_options(fetch_opts).clone(repo_url.to_string().as_str(), &checkout_dir)
    }?;

    let rev = match repo.revparse_single(vcs_revision) {
        Ok(rev) => rev,
        Err(err) => {
            if err.code() == git2::ErrorCode::NotFound {
                bail!("Revision {} not found in {}", vcs_revision, repo_url)
            } else {
                Err(err)?
            }
        }
    };

    println!("git checkout: rev={}", rev.short_id()?.as_str().unwrap());
    let mut cb = CheckoutBuilder::new();

    // TODO: create named branch for checkout of specific version
    repo.checkout_tree(&rev, Some(&mut cb))?;

    Ok(())
}

async fn unpack_crate_archive(tmpdir: &TempDir, package_name: &str, package_version: &semver::Version) -> Result<PathBuf> {
    let mut archive = fetch_crate_archive(
        package_name,
        package_version).await?;

    archive.unpack(&tmpdir).await?;

    // The archive is expected to have a single top-level directory like "package-1.2.3"
    // Ensure this is true, and return that directory name
    let mut cur_path = None;
    for entry in fs::read_dir(&tmpdir)? {
        let entry = entry?;
        if let Some(_) = cur_path {
            bail!("More than one root entry in crate archive for {}-{}", package_name, package_version);
        } else {
            cur_path = Some(entry.path());
        }
    }
    Ok(cur_path.ok_or_else(|| anyhow!("Empty crate archive"))?)
}


#[tokio::main]
async fn main() -> Result<()> {

    let args = Args::parse();

    // TOOD: this assumes running from package root
    let crate_root = PathBuf::from(".");
    let metadata_before = query_metadata(&crate_root)?;

    let workspace_root = metadata_before.workspace_root.as_std_path();
    let manifest_path = workspace_root.join("Cargo.toml");

    let mut manifest: Manifest = {
        let mf_contents = std::fs::read_to_string(&manifest_path).with_context(|| "Failed to read manifest contents")?;
        mf_contents.parse().with_context(|| "Failed to parse manifest")?
    };
    println!("Using manifest: {}", manifest_path.display());

    let package_meta = metadata_before.packages.iter()
        .find(|pkg| pkg.name == args.crate_name)
        .ok_or_else(|| {
            anyhow!("Package {} is not a dependency", args.crate_name)
        })?;

    let workspace_parent= workspace_root.parent().unwrap();

    let patch_dir = if args.source == Source::Crate {

        let tmpdir = tempfile::TempDir::new()?;
        let archive_root = unpack_crate_archive(&tmpdir, &args.crate_name, &package_meta.version).await?;

        let patch_root = args.dest_dir.unwrap_or_else(|| workspace_parent.join(archive_root.file_name().unwrap()));
        if patch_root.try_exists()? {
            bail!("Patch directory {} already exists", patch_root.display());
        }
        fs::rename(&archive_root, &patch_root)?;
        patch_root
    } else {
        let repo_url = crate_get_repo(&args.crate_name).await?;

        let final_segment = repo_url.path_segments().unwrap().last().unwrap();

        let checkout_dir = args.dest_dir.unwrap_or_else(|| workspace_parent.join(&final_segment));

        let current_vcs_info = lookup_vcs_revision(&args.crate_name, &package_meta.version).await?;

        let revision = if args.source == Source::VCSHead {
            "HEAD"
        } else {
            let vcinfo = current_vcs_info
            .as_ref()
            .ok_or_else(|| anyhow!(
                "No .cargo_vcs_info.json for package {}:{}, try --source-vcs-head",
                args.crate_name, package_meta.version))?;
            &vcinfo.hash
        };

        git_checkout(&repo_url, revision, &checkout_dir)?;

        let path_in_repo = if let Some(vcs_info) = current_vcs_info {
            vcs_info.path_in_repo
        } else {
            let dep_metadata = query_metadata(&checkout_dir)?;
            let pkg = dep_metadata.workspace_packages().into_iter().find(|p| p.name == args.crate_name)
                .ok_or_else(|| anyhow!("Failed to find package {} in repo {}", args.crate_name, checkout_dir.display()))?;

            pkg.manifest_path.parent().unwrap()
                .strip_prefix(&checkout_dir).unwrap()
                .to_path_buf().into_std_path_buf()
        };

        checkout_dir.join(path_in_repo)
    };

    manifest_insert_patch(&mut manifest, &args.crate_name, &patch_dir)?;
    println!("writing manifest: {}", manifest_path.display());

    let s = manifest.data.to_string();
    let new_contents_bytes = s.as_bytes();

    std::fs::write(&manifest_path, new_contents_bytes).context("Failed to write updated Cargo.toml")?;

    Command::new("cargo")
        .args(&[
            "update", "--quiet",
            "--workspace",
            "--package", &args.crate_name,
            "--manifest-path", &manifest_path.to_str().unwrap()])
        .current_dir(&workspace_root)
        .status()?;

    let metadata_after = query_metadata(&crate_root)?;
    print_changed_deps(&metadata_before, &metadata_after);

    Ok(())
}

