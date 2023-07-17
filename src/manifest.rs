use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::*;
use cargo_edit::Manifest;
use cargo_metadata::{Metadata, Package, PackageId};
use toml_edit::{InlineTable, Item, Table, Value};

pub(crate) fn diff_deps<'a>(
    metadata_before: &'a Metadata,
    metadata_after: &'a Metadata,
) -> Vec<(&'a PackageId, (Vec<&'a PackageId>, Vec<&'a PackageId>))> {
    /// Given the output of `cargo metadata`, return a map from PackageId to the set of PackageIds it depends on
    fn cargo_meta_to_depmap(metadata: &Metadata) -> HashMap<&PackageId, HashSet<&PackageId>> {
        let resolve = metadata.resolve.as_ref().expect("metadata resolve missing");
        HashMap::from_iter(
            resolve
                .nodes
                .iter()
                .map(|node| (&node.id, node.dependencies.iter().collect())),
        )
    }

    let depmap_after = cargo_meta_to_depmap(&metadata_after);
    let depmap_before = cargo_meta_to_depmap(&metadata_before);

    let diffs: Vec<_> = depmap_after
        .iter()
        .filter_map(|(pkg_id, deps_after)| {
            let diff = if let Some(deps_before) = depmap_before.get(pkg_id) {
                if deps_before == deps_after {
                    return None;
                }
                let del_dep = deps_before.difference(deps_after);
                let add_dep = deps_after.difference(deps_before);
                (
                    *pkg_id,
                    (
                        del_dep.into_iter().map(|d| *d).collect(),
                        add_dep.into_iter().map(|d| *d).collect(),
                    ),
                )
            } else {
                (*pkg_id, (Vec::new(), Vec::new()))
            };
            Some(diff)
        })
        .collect();
    diffs
}

fn toml_get_or_create_table_by_path<'a>(
    path: &[&str],
    table: &'a mut Table,
) -> anyhow::Result<&'a mut Table> {
    let mut current = table;
    for key in path {
        current = current
            .entry(key)
            .or_insert_with(|| toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| anyhow!("Maformed Cargo.toml"))?;
        current.set_implicit(true);
    }
    Ok(current)
}

pub(crate) fn manifest_insert_patch(
    manifest: &mut Manifest,
    package_name: &str,
    patch_dir: &Path,
) -> Result<()> {
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

pub(crate) fn query_metadata(cdir: &Path) -> Result<Metadata> {
    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.current_dir(cdir);
    Ok(metadata_cmd.exec()?)
}

pub(crate) fn parse_manifest(manifest_path: &Path) -> Result<Manifest> {
    let mf_contents = std::fs::read_to_string(&manifest_path)
        .with_context(|| "Failed to read manifest contents")?;
    mf_contents
        .parse::<Manifest>()
        .with_context(|| "Failed to parse manifest")
}

pub(crate) fn find_package<'a>(
    packages: impl Iterator<Item = &'a Package>,
    package_name: &str,
) -> Result<&'a Package> {
    let pkgs: Vec<_> = packages.filter(|p| p.name == package_name).collect();
    match pkgs.len() {
        0 => bail!("failed to find package {}", package_name),
        1 => Ok(pkgs[0]),
        _ => bail!("found multiple packages with name {}", package_name),
    }
}
