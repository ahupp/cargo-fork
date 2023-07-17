///
///
mod crates_io;
mod manifest;

use anyhow::{anyhow, bail, Context, Result};

use clap::Parser;

use git2::{
    build::{CheckoutBuilder, RepoBuilder},
    FetchOptions, RemoteCallbacks, Repository,
};
use reqwest::Url;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    crates_io::crate_get_repo,
    manifest::{find_package, parse_manifest, query_metadata},
};
use crate::{crates_io::lookup_vcs_for_version, manifest::manifest_insert_patch};
use crate::{crates_io::unpack_crate_archive, manifest::diff_deps};

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
        RepoBuilder::new()
            .fetch_options(fetch_opts)
            .clone(repo_url.to_string().as_str(), &checkout_dir)
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // TOOD: this assumes running from package root
    let crate_root = PathBuf::from(".");
    let metadata_before = query_metadata(&crate_root)?;

    let workspace_root = metadata_before.workspace_root.as_std_path();
    let manifest_path = workspace_root.join("Cargo.toml");

    let mut manifest = parse_manifest(&manifest_path)?;
    println!("Using manifest: {}", manifest_path.display());

    // TODO: handle multiple versions in dependency tree
    let package_meta = find_package(metadata_before.packages.iter(), &args.crate_name)?;

    let workspace_parent = workspace_root.parent().unwrap();

    let patch_dir = if args.source == Source::Crate {
        let tmpdir = tempfile::TempDir::new()?;
        let archive_root =
            unpack_crate_archive(&tmpdir, &args.crate_name, &package_meta.version).await?;

        let patch_root = args
            .dest_dir
            .unwrap_or_else(|| workspace_parent.join(&archive_root));
        if patch_root.try_exists()? {
            bail!("Patch directory {} already exists", patch_root.display());
        }
        fs::rename(&tmpdir.path().join(&archive_root), &patch_root)?;
        patch_root
    } else {
        let repo_url = crate_get_repo(&args.crate_name).await?;

        // https://github.com/ahupp/cargo-fork -> cargo-fork
        let final_segment = repo_url.path_segments().unwrap().last().unwrap();

        let checkout_dir = args
            .dest_dir
            .unwrap_or_else(|| workspace_parent.join(&final_segment));

        let vcs_info = lookup_vcs_for_version(&args.crate_name, &package_meta.version).await?;

        let revision = if args.source == Source::VCSHead {
            "HEAD"
        } else {
            let vcinfo = vcs_info.as_ref().ok_or_else(|| {
                anyhow!(
                    "No .cargo_vcs_info.json for package {}:{}, try --source-vcs-head",
                    args.crate_name,
                    package_meta.version
                )
            })?;
            &vcinfo.hash
        };

        git_checkout(&repo_url, revision, &checkout_dir)?;

        let path_in_repo = if let Some(vcs_info) = vcs_info {
            vcs_info.path_in_vcs
        } else {
            // Infer path in vcs when package is in a workspace and path_in_vcs is not specified in crate_vcs_info.json
            let dep_metadata = query_metadata(&checkout_dir)?;
            let pkg = find_package(
                // Would be nice for references to be handled consistantly
                // without this explicit deref
                dep_metadata.workspace_packages().iter().map(|p| *p),
                &args.crate_name,
            )?;

            pkg.manifest_path
                .parent()
                .unwrap()
                .strip_prefix(&checkout_dir)
                .unwrap()
                .to_path_buf()
                .into_std_path_buf()
        };

        checkout_dir.join(path_in_repo)
    };

    manifest_insert_patch(&mut manifest, &args.crate_name, &patch_dir)?;
    println!("writing manifest: {}", manifest_path.display());

    std::fs::write(&manifest_path, manifest.data.to_string().as_bytes())
        .context("Failed to write updated Cargo.toml")?;

    Command::new("cargo")
        .args(&[
            "update",
            "--quiet",
            "--workspace",
            "--package",
            &args.crate_name,
            "--manifest-path",
            &manifest_path.to_str().unwrap(),
        ])
        .current_dir(&workspace_root)
        .status()?;

    let metadata_after = query_metadata(&crate_root)?;
    let diffs = diff_deps(&metadata_before, &metadata_after);

    for (pkg_id, (del_dep, add_dep)) in diffs {
        println!("{}:", pkg_id);
        for d in del_dep {
            println!("  - {}", d);
        }
        for a in add_dep {
            println!("  + {}", a);
        }
    }

    // TODO: detect if patch is unused anywhere, and flag it noisily

    Ok(())
}
