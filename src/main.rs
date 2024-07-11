/*
 * Copyright 2016-2017 Doug Goldstein <cardoe@cardoe.com>
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

extern crate anyhow;
extern crate cargo;
extern crate git2;
extern crate itertools;
extern crate lazy_static;
extern crate md5;
extern crate regex;
extern crate structopt;

use anyhow::{anyhow, Context as _};
use cargo::core::resolver::CliFeatures;
use cargo::core::{resolver::features::HasDevUnits, MaybePackage};
use cargo::core::{GitReference, Package, PackageSet, Resolve, Workspace};
use cargo::ops;
use cargo::util::{important_paths, CargoResult};
use cargo::{core::registry::PackageRegistry, sources::CRATES_IO_DOMAIN};
use cargo::{CliResult, GlobalContext};
use itertools::Itertools;
use semver::Version;
use std::default::Default;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use structopt::clap::AppSettings;
use structopt::StructOpt;

mod git;
mod license;

struct Metadata<'cfg> {
    name: &'cfg str,
    version: Version,
    description: Option<&'cfg str>,
    homepage: Option<&'cfg str>,
    repository: Option<&'cfg str>,
    license: Option<&'cfg str>,
    license_file: Option<&'cfg str>,
}

impl<'cfg> Metadata<'cfg> {
    fn load(ws: &'cfg Workspace<'cfg>) -> CargoResult<Self> {
        match ws.root_maybe() {
            MaybePackage::Virtual(virt) => {
                let metadata = virt
                    .resolved_toml()
                    .workspace
                    .as_ref()
                    .context("missing 'workspace' table")?
                    .metadata
                    .as_ref()
                    .context("missing 'workspace.metadata' table")?
                    .as_table()
                    .context("'workspace.metadata' must be a table")?;
                let get_str = |field_name| -> CargoResult<&str> {
                    metadata
                        .get(field_name)
                        .with_context(|| {
                            format!("missing '{field_name}' field in 'workspace.metadata'")
                        })?
                        .as_str()
                        .context("'workspace.metadata.name' must be a string")
                };
                let get_str_opt = |field_name| -> CargoResult<Option<&str>> {
                    metadata
                        .get(field_name)
                        .map(|field| {
                            field
                                .as_str()
                                .context("'workspace.metadata.name' must be a string")
                        })
                        .transpose()
                };

                Ok(Self {
                    name: get_str("name")?,
                    version: get_str("version")?.parse()?,
                    description: get_str_opt("description")?,
                    homepage: get_str_opt("homepage")?,
                    repository: get_str_opt("repository")?,
                    license: get_str_opt("license")?,
                    license_file: get_str_opt("license-file")?,
                })
            }
            MaybePackage::Package(pkg) => {
                let metadata = pkg.manifest().metadata();
                Ok(Self {
                    name: pkg.name().as_str(),
                    version: pkg.version().clone(),
                    description: metadata.description.as_deref(),
                    homepage: metadata.homepage.as_deref(),
                    repository: metadata.repository.as_deref(),
                    license: metadata.license.as_deref(),
                    license_file: metadata.license_file.as_deref(),
                })
            }
        }
    }
}

/// Represents the package we are trying to generate a recipe for
struct Project<'cfg> {
    cfg: &'cfg GlobalContext,
    current_manifest: PathBuf,
    ws: Workspace<'cfg>,
}

impl<'cfg> Project<'cfg> {
    /// creates our package info from the config and the `manifest_path`,
    /// which may not be provided
    fn new(config: &GlobalContext, manifest_path: Option<String>) -> CargoResult<Project> {
        let manifest_path = manifest_path.map_or_else(|| config.cwd().to_path_buf(), PathBuf::from);
        let root = important_paths::find_root_manifest_for_wd(&manifest_path)?;
        let ws = Workspace::new(&root, config)?;
        Ok(Project {
            cfg: config,
            current_manifest: root,
            ws,
        })
    }

    /// Returns the set of all packages in the workspace.
    fn packages(&self) -> Vec<&Package> {
        self.ws.members().collect()
    }

    /// Generates a package registry by using the Cargo.lock or
    /// creating one as necessary
    fn registry(&self, packages: &[&Package]) -> CargoResult<PackageRegistry<'cfg>> {
        let mut registry = PackageRegistry::new(self.cfg)?;
        let source_ids = packages
            .iter()
            .map(|package| package.package_id().source_id());
        registry.add_sources(source_ids)?;
        Ok(registry)
    }

    /// Resolve the packages necessary for the workspace
    fn resolve(&self, packages: &[&Package]) -> CargoResult<(PackageSet<'cfg>, Resolve)> {
        // build up our registry
        let mut registry = self.registry(packages)?;

        // resolve our dependencies
        let (packages, resolve) = ops::resolve_ws(&self.ws)?;

        // resolve with all features set so we ensure we get all of the depends downloaded
        let resolve = ops::resolve_with_previous(
            &mut registry,
            &self.ws,
            /* resolve it all */
            &CliFeatures::new_all(true),
            HasDevUnits::No,
            /* previous */
            Some(&resolve),
            /* don't avoid any */
            None,
            /* specs */
            &[],
            /* warn? */
            true,
        )?;

        Ok((packages, resolve))
    }

    /// packages that are part of a workspace are a sub directory from the
    /// top level which we need to record, this provides us with that
    /// relative directory
    fn rel_dir(&self) -> CargoResult<PathBuf> {
        // this is the top level of the workspace
        let root = self.ws.root().to_path_buf();
        // path where our current package's Cargo.toml lives
        let cwd = self.current_manifest.parent().ok_or_else(|| {
            anyhow!(
                "Could not get parent of directory '{}'",
                self.current_manifest.display()
            )
        })?;

        cwd.strip_prefix(&root)
            .map(Path::to_path_buf)
            .context("Unable to if Cargo.toml is in a sub directory")
    }
}

#[derive(StructOpt, Debug)]
struct Args {
    /// Silence all output
    #[structopt(short = "q")]
    quiet: bool,

    /// Verbose mode (-v, -vv, -vvv, etc.)
    #[structopt(short = "v", parse(from_occurrences))]
    verbose: usize,

    /// Reproducible mode: Output exact git references for git projects
    #[structopt(short = "R")]
    reproducible: bool,

    /// Legacy Overrides: Use legacy override syntax
    #[structopt(short = "l", long = "--legacy-overrides")]
    legacy_overrides: bool,
}

#[derive(StructOpt, Debug)]
#[structopt(
    name = "cargo-bitbake",
    bin_name = "cargo",
    author,
    about = "Generates a BitBake recipe for a given Cargo project",
    global_settings(&[AppSettings::ColoredHelp])
)]
enum Opt {
    /// Generates a BitBake recipe for a given Cargo project
    #[structopt(name = "bitbake")]
    Bitbake(Args),
}

fn main() {
    let mut config = GlobalContext::default().unwrap();
    let Opt::Bitbake(opt) = Opt::from_args();
    let result = real_main(opt, &mut config);
    if let Err(e) = result {
        cargo::exit_with_error(e, &mut *config.shell());
    }
}

fn real_main(options: Args, config: &mut GlobalContext) -> CliResult {
    config.configure(
        options.verbose as u32,
        options.quiet,
        /* color */
        None,
        /* frozen */
        false,
        /* locked */
        false,
        /* offline */
        false,
        /* target dir */
        &None,
        /* unstable flags */
        &[],
        /* CLI config */
        &[],
    )?;

    // Build up data about the package we are attempting to generate a recipe for
    let project = Project::new(config, None)?;
    let metadata = Metadata::load(&project.ws)?;

    if metadata.name.contains('_') {
        println!("Project name contains an underscore");
    }

    // All packages in the workspace
    let ws_packages = project.packages();
    // Resolve all dependencies (generate or use Cargo.lock as necessary)
    let (_, resolve) = project.resolve(&ws_packages)?;

    // build the crate URIs
    let mut src_uri_extras = vec![];
    let mut src_uris = resolve
        .iter()
        .filter_map(|pkg| {
            // get the source info for this package
            let src_id = pkg.source_id();
            if ws_packages.iter().any(|ws_pkg| ws_pkg.name() == pkg.name()) {
                None
            } else if src_id.is_crates_io() {
                // this package appears in a crate registry
                if let Some(Some(csum)) = resolve.checksums().get(&pkg) {
                    src_uri_extras.push(format!(
                        "SRC_URI[{name}-{version}.sha256sum] = \"{csum}\"",
                        name = pkg.name(),
                        version = pkg.version()
                    ));
                }
                Some(format!(
                    "    crate://{}/{}/{} \\\n",
                    CRATES_IO_DOMAIN,
                    pkg.name(),
                    pkg.version()
                ))
            } else if src_id.is_path() {
                // we don't want to spit out path based
                // entries since they're within the crate
                // we are packaging
                None
            } else if src_id.is_git() {
                // Just use the default download method for git repositories
                // found in the source URIs, since cargo currently cannot
                // initialize submodules for git dependencies anyway.
                let url = git::git_to_yocto_git_url(
                    src_id.url().as_str(),
                    Some(pkg.name().as_str()),
                    git::GitPrefix::default(),
                );

                // save revision
                src_uri_extras.push(format!("SRCREV_FORMAT .= \"_{}\"", pkg.name()));

                let precise = if options.reproducible {
                    src_id.precise_git_fragment()
                } else {
                    None
                };

                let rev = if let Some(precise) = precise {
                    precise
                } else {
                    match *src_id.git_reference()? {
                        GitReference::Tag(ref s) => s,
                        GitReference::Rev(ref s) => {
                            if s.len() == 40 {
                                // avoid reduced hashes
                                s
                            } else {
                                let precise = src_id.precise_git_fragment();
                                if let Some(p) = precise {
                                    p
                                } else {
                                    panic!("cannot find rev in correct format!");
                                }
                            }
                        }
                        GitReference::Branch(ref s) => {
                            if s == "master" {
                                "${AUTOREV}"
                            } else {
                                s
                            }
                        }
                        GitReference::DefaultBranch => "${AUTOREV}",
                    }
                };

                src_uri_extras.push(format!("SRCREV_{} = \"{}\"", pkg.name(), rev));
                // instruct Cargo where to find this
                src_uri_extras.push(format!(
                    "EXTRA_OECARGO_PATHS += \"${{WORKDIR}}/{}\"",
                    pkg.name()
                ));

                Some(format!("    {} \\\n", url))
            } else {
                Some(format!("    {} \\\n", src_id.url()))
            }
        })
        .collect::<Vec<String>>();

    // sort the crate list
    src_uris.sort();
    src_uri_extras.sort();

    // package description is used as BitBake summary
    let summary = metadata.description.unwrap_or_else(|| {
        println!("No 'description' field set in your Cargo.toml, using 'name' field");
        metadata.name
    });

    // package homepage (or source code location)
    let homepage = metadata
        .homepage
        .map_or_else(
            || {
                println!("No 'homepage' field set in your Cargo.toml, trying 'repository' field");
                metadata
                    .repository
                    .ok_or_else(|| anyhow!("No 'repository' field set in your Cargo.toml"))
            },
            Ok,
        )?
        .trim();

    // package license
    let license = metadata.license.unwrap_or_else(|| {
        println!("No 'license' field set in your Cargo.toml, trying 'license-file' field");
        metadata.license_file.unwrap_or_else(|| {
            println!("No 'license-file' field set in your Cargo.toml");
            println!("Assuming {} license", license::CLOSED_LICENSE);
            license::CLOSED_LICENSE
        })
    });

    // compute the relative directory into the repo our Cargo.toml is at
    let rel_dir = project.rel_dir()?;

    // license files for the package
    let mut lic_files = vec![];
    let licenses: Vec<&str> = license.split('/').collect();
    let single_license = licenses.len() == 1;
    for lic in licenses {
        lic_files.push(format!(
            "    {}",
            license::file(project.ws.root(), &rel_dir, lic, single_license)
        ));
    }

    // license data in Yocto fmt
    let license = license.split('/').map(str::trim).join(" | ");

    // attempt to figure out the git repo for this project
    let project_repo = git::ProjectRepo::new(config).unwrap_or_else(|e| {
        println!("{}", e);
        Default::default()
    });

    // if this is not a tag we need to include some data about the version in PV so that
    // the sstate cache remains valid
    let git_srcpv = if !project_repo.tag && project_repo.rev.len() > 10 {
        let mut pv_append_key = "PV:append";
        // Override PV override with legacy syntax if flagged
        if options.legacy_overrides {
            pv_append_key = "PV_append";
        }
        // we should be using ${SRCPV} here but due to a bitbake bug we cannot. see:
        // https://github.com/meta-rust/meta-rust/issues/136
        format!(
            "{} = \".AUTOINC+{}\"",
            pv_append_key,
            &project_repo.rev[..10],
        )
    } else {
        // its a tag so nothing needed
        "".into()
    };

    // build up the path
    let recipe_path = PathBuf::from(format!(
        "{name}_{version}.bb",
        name = metadata.name,
        version = metadata.version,
    ));

    // Open the file where we'll write the BitBake recipe
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&recipe_path)
        .map_err(|e| anyhow!("Unable to open bitbake recipe file with: {}", e))?;

    // write the contents out
    write!(
        file,
        include_str!("bitbake.template"),
        name = metadata.name,
        version = metadata.version,
        summary = summary,
        homepage = homepage,
        license = license,
        lic_files = lic_files.join(""),
        src_uri = src_uris.join(""),
        src_uri_extras = src_uri_extras.join("\n"),
        project_rel_dir = rel_dir.display(),
        project_src_uri = project_repo.uri,
        project_src_rev = project_repo.rev,
        git_srcpv = git_srcpv,
        cargo_bitbake_ver = env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| anyhow!("Unable to write to bitbake recipe file with: {}", e))?;

    println!("Wrote: {}", recipe_path.display());

    Ok(())
}
