/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod mem_writes_changesets;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Error;
use blobrepo::BlobRepo;
use blobrepo_override::DangerousOverride;
use blobstore::Blobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::ArcBonsaiHgMapping;
use bonsai_hg_mapping::MemWritesBonsaiHgMapping;
use cacheblob::dummy::DummyLease;
use cacheblob::LeaseOps;
use cacheblob::MemWritesBlobstore;
use changesets::ArcChangesets;
use clap::Parser;
use clap::Subcommand;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::future;
use import_tools::import_tree_as_single_bonsai_changeset;
use import_tools::GitimportPreferences;
use import_tools::GitimportTarget;
use linked_hash_map::LinkedHashMap;
use mercurial_derived_data::get_manifest_from_bonsai;
use mercurial_derived_data::DeriveHgChangeset;
use mononoke_app::args::RepoArgs;
use mononoke_app::fb303::AliveService;
use mononoke_app::fb303::Fb303AppExtension;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_types::ChangesetId;
use slog::info;

use crate::mem_writes_changesets::MemWritesChangesets;

// Refactor this a bit. Use a thread pool for git operations. Pass that wherever we use store repo.
// Transform the walk into a stream of commit + file changes.

async fn derive_hg(
    ctx: &CoreContext,
    repo: &BlobRepo,
    import_map: impl Iterator<Item = (&git_hash::ObjectId, &ChangesetId)>,
) -> Result<(), Error> {
    let mut hg_manifests = HashMap::new();

    for (id, bcs_id) in import_map {
        let bcs = bcs_id.load(ctx, repo.blobstore()).await?;
        let parent_manifests = future::try_join_all(bcs.parents().map({
            let hg_manifests = &hg_manifests;
            move |p| async move {
                let manifest = if let Some(manifest) = hg_manifests.get(&p) {
                    *manifest
                } else {
                    repo.derive_hg_changeset(ctx, p)
                        .await?
                        .load(ctx, repo.blobstore())
                        .await?
                        .manifestid()
                };
                Result::<_, Error>::Ok(manifest)
            }
        }))
        .await?;

        let manifest = get_manifest_from_bonsai(
            ctx.clone(),
            repo.get_blobstore().boxed(),
            bcs.clone(),
            parent_manifests,
        )
        .await?;

        hg_manifests.insert(*bcs_id, manifest);

        info!(ctx.logger(), "Hg: {:?}: {:?}", id, manifest);
    }

    Ok(())
}

/// Mononoke Git Importer
#[derive(Parser)]
struct GitimportArgs {
    #[clap(long)]
    derive_hg: bool,
    /// This is used to suppress the printing of the potentially really long git Reference -> BonzaiID mapping.
    #[clap(long)]
    suppress_ref_mapping: bool,
    /// Set the path to the git binary - preset to git.real
    #[clap(long)]
    git_command_path: Option<String>,
    /// Path to a git repository to import
    git_repository_path: String,
    /// Reupload git commits, even if they already exist in Mononoke
    #[clap(long)]
    reupload_commits: bool,
    #[clap(subcommand)]
    subcommand: GitimportSubcommand,
    #[clap(flatten)]
    repo_args: RepoArgs,
}

#[derive(Subcommand)]
enum GitimportSubcommand {
    FullRepo,
    GitRange { git_from: String, git_to: String },
    MissingForCommit { git_commit: String },
    ImportTreeAsSingleBonsaiChangeset { git_commit: String },
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let app = MononokeAppBuilder::new(fb)
        .with_app_extension(Fb303AppExtension {})
        .build::<GitimportArgs>()?;

    app.run_with_fb303_monitoring(async_main, "gitimport", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<(), Error> {
    let logger = app.logger();
    let ctx = CoreContext::new_with_logger(app.fb, logger.clone());
    let args: GitimportArgs = app.args()?;
    let mut prefs = GitimportPreferences::default();

    // if we are readonly, then we'll set up some overrides to still be able to do meaningful
    // things below.
    let dry_run = app.readonly_storage().0;
    prefs.dry_run = dry_run;

    if let Some(path) = args.git_command_path {
        prefs.git_command_path = PathBuf::from(path);
    }

    let path = Path::new(&args.git_repository_path);

    let reupload = if args.reupload_commits {
        import_direct::ReuploadCommits::Always
    } else {
        import_direct::ReuploadCommits::Never
    };

    let repo: BlobRepo = app.open_repo(&args.repo_args).await?;
    info!(
        logger,
        "using repo \"{}\" repoid {:?}",
        repo.name(),
        repo.get_repoid(),
    );

    let repo = if dry_run {
        repo.dangerous_override(|blobstore| -> Arc<dyn Blobstore> {
            Arc::new(MemWritesBlobstore::new(blobstore))
        })
        .dangerous_override(|changesets| -> ArcChangesets {
            Arc::new(MemWritesChangesets::new(changesets))
        })
        .dangerous_override(|bonsai_hg_mapping| -> ArcBonsaiHgMapping {
            Arc::new(MemWritesBonsaiHgMapping::new(bonsai_hg_mapping))
        })
        .dangerous_override(|_| Arc::new(DummyLease {}) as Arc<dyn LeaseOps>)
    } else {
        repo
    };

    let uploader = import_direct::DirectUploader::new(repo.clone(), reupload);

    let target = match args.subcommand {
        GitimportSubcommand::FullRepo {} => GitimportTarget::full(),
        GitimportSubcommand::GitRange { git_from, git_to } => {
            let from = git_from.parse()?;
            let to = git_to.parse()?;
            import_direct::range(from, to, &ctx, &repo).await?
        }
        GitimportSubcommand::MissingForCommit { git_commit } => {
            let commit = git_commit.parse()?;
            import_direct::missing_for_commit(commit, &ctx, &repo, &prefs.git_command_path, path)
                .await?
        }
        GitimportSubcommand::ImportTreeAsSingleBonsaiChangeset { git_commit } => {
            let commit = git_commit.parse()?;
            let bcs_id =
                import_tree_as_single_bonsai_changeset(&ctx, path, uploader, commit, &prefs)
                    .await?;
            info!(ctx.logger(), "imported as {}", bcs_id);
            if args.derive_hg {
                derive_hg(&ctx, &repo, [(&commit, &bcs_id)].into_iter()).await?;
            }
            return Ok(());
        }
    };

    let gitimport_result: LinkedHashMap<_, _> =
        import_tools::gitimport(&ctx, path, uploader, &target, &prefs).await?;
    if args.derive_hg {
        derive_hg(&ctx, &repo, gitimport_result.iter()).await?;
    }

    if !args.suppress_ref_mapping {
        let refs = import_tools::read_git_refs(path, &prefs).await?;
        for (name, commit) in refs {
            let bcs_id = gitimport_result.get(&commit);
            info!(
                ctx.logger(),
                "Ref: {:?}: {:?}",
                String::from_utf8_lossy(&name),
                bcs_id
            );
        }
    }

    Ok(())
}
