/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::Ordering;
/// Union and intersection can be made more efficient if the streams are uninterrupted streams of
/// ancestors. For example:
///
/// A-o   o-B
///    \ /
///     o - C
///     |
///     o
///     |
///    ...
///
/// UnionNodeStream(A, B) would poll both streams until they are exhausted. That means that node C
/// and all of its ancestors would be generated twice. This is not necessary.
/// For IntersectNodeStream(A, B) the problem is even more acute. The stream will return just one
/// entry, however it will generate all ancestors of A and B twice, and there can be lots of them!
///
/// The stream below aims to solve the aforementioned problems. It's primary usage is in
/// Mercurial pull to find commits that need to be sent to a client.
use std::collections::hash_set::IntoIter;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::iter;
use std::sync::Arc;

use anyhow::Error;
use changeset_fetcher::ArcChangesetFetcher;
use cloned::cloned;
use context::CoreContext;
use futures_ext::BoxFuture;
use futures_ext::BoxStream;
use futures_ext::FutureExt as FBFutureExt;
use futures_ext::SelectAll;
use futures_ext::StreamExt;
use futures_old::future::ok;
use futures_old::future::Future;
use futures_old::stream;
use futures_old::stream::iter_ok;
use futures_old::stream::Stream;
use futures_old::try_ready;
use futures_old::Async;
use futures_old::IntoFuture;
use futures_old::Poll;
use futures_util::future::FutureExt;
use futures_util::future::TryFutureExt;
use maplit::hashset;
use mononoke_types::ChangesetId;
use mononoke_types::Generation;
use reachabilityindex::LeastCommonAncestorsHint;
use reachabilityindex::NodeFrontier;

use crate::errors::*;
use crate::setcommon::*;
use crate::BonsaiNodeStream;
use crate::UniqueHeap;

/// As the name suggests, it's a difference of unions of ancestors of nodes.
/// In mercurial revset's terms it's (::A) - (::B), where A and B are sets of nodes.
/// In Mononoke revset's terms it's equivalent to
///
/// ```ignore
///   let include: Vec<HgNodeHash> = vec![ ... ];
///   let exclude: Vec<HgNodeHash> = vec![ ... ];
///   ...
///   let mut include_ancestors = vec![];
///   for i in include.clone() {
///     include_ancestors.push(
///         AncestorsNodeStream::new(&repo, repo_generation.clone(), i).boxify()
///     );
///   }
///
///   let mut exclude_ancestors = vec![];
///   for i in exclude.clone() {
///     exclude_ancestors.push(
///         AncestorsNodeStream::new(&repo, repo_generation.clone(), i).boxify()
///     );
///   }
///
///   let include_ancestors = UnionNodeStream::new(
///     &repo, repo_generation.clone(), include_ancestors
///   ).boxify();
///   let exclude_ancestors = UnionNodeStream::new(
///     &repo, repo_generation.clone(), exclude_ancestors
///   ).boxify();
///   let expected =
///     SetDifferenceNodeStream::new(
///         &repo, repo_generation.clone(), include_ancestors, exclude_ancestors
///    );
/// ```
///

pub struct DifferenceOfUnionsOfAncestorsNodeStream {
    ctx: CoreContext,

    changeset_fetcher: ArcChangesetFetcher,

    // Given a set "nodes", and a maximum generation "gen",
    // return a set of nodes "C" which satisfies:
    // - Max generation number in "C" is <= gen
    // - Any ancestor of "nodes" with generation <= gen is also an ancestor of "C"
    // It's used to move `exclude` NodeFrontier
    lca_hint_index: Arc<dyn LeastCommonAncestorsHint>,

    // Nodes that we know about, grouped by generation.
    next_generation: BTreeMap<Generation, HashSet<ChangesetId>>,

    // The generation of the nodes in `drain`. All nodes with bigger generation has already been
    // returned
    current_generation: Generation,

    // Parents of entries from `drain`. We fetch generation number for them.
    pending_changesets: SelectAll<BoxStream<(ChangesetId, Generation), Error>>,

    // Stream of (Hashset, Generation) that needs to be excluded
    exclude_ancestors_future: BoxFuture<NodeFrontier, Error>,
    current_exclude_generation: Option<Generation>,

    // Nodes which generation is equal to `current_generation`. They will be returned from the
    // stream unless excluded.
    drain: iter::Peekable<IntoIter<ChangesetId>>,

    // max heap of all relevant unique generation numbers  for include nodes
    sorted_unique_generations: UniqueHeap<Generation>,
}

fn make_pending(
    ctx: CoreContext,
    changeset_fetcher: ArcChangesetFetcher,
    hash: ChangesetId,
) -> BoxStream<(ChangesetId, Generation), Error> {
    let new_repo_changesets = changeset_fetcher.clone();
    let new_repo_gennums = changeset_fetcher.clone();

    Ok::<_, Error>(hash)
        .into_future()
        .and_then({
            cloned!(ctx);
            move |hash| {
                async move { new_repo_changesets.get_parents(ctx, hash).await }
                    .boxed()
                    .compat()
                    .map(|parents| parents.into_iter())
                    .map_err(|err| err.context(ErrorKind::ParentsFetchFailed))
            }
        })
        .map(iter_ok::<_, Error>)
        .flatten_stream()
        .and_then(move |node_hash| {
            cloned!(ctx, new_repo_gennums);
            async move { new_repo_gennums.get_generation_number(ctx, node_hash).await }
                .boxed()
                .compat()
                .map(move |gen_id| (node_hash, gen_id))
                .map_err(|err| err.context(ErrorKind::GenerationFetchFailed))
        })
        .boxify()
}

impl DifferenceOfUnionsOfAncestorsNodeStream {
    pub fn new(
        ctx: CoreContext,
        changeset_fetcher: &ArcChangesetFetcher,
        lca_hint_index: Arc<dyn LeastCommonAncestorsHint>,
        hash: ChangesetId,
    ) -> BonsaiNodeStream {
        Self::new_with_excludes(ctx, changeset_fetcher, lca_hint_index, vec![hash], vec![])
    }

    pub fn new_union(
        ctx: CoreContext,
        changeset_fetcher: &ArcChangesetFetcher,
        lca_hint_index: Arc<dyn LeastCommonAncestorsHint>,
        hashes: Vec<ChangesetId>,
    ) -> BonsaiNodeStream {
        Self::new_with_excludes(ctx, changeset_fetcher, lca_hint_index, hashes, vec![])
    }

    pub fn new_with_excludes(
        ctx: CoreContext,
        changeset_fetcher: &ArcChangesetFetcher,
        lca_hint_index: Arc<dyn LeastCommonAncestorsHint>,
        hashes: Vec<ChangesetId>,
        excludes: Vec<ChangesetId>,
    ) -> BonsaiNodeStream {
        let changeset_fetcher = changeset_fetcher.clone();
        add_generations_by_bonsai(
            ctx.clone(),
            stream::iter_ok(hashes.into_iter()).boxify(),
            changeset_fetcher.clone(),
        )
        .collect()
        .join(
            add_generations_by_bonsai(
                ctx.clone(),
                stream::iter_ok(excludes.into_iter()).boxify(),
                changeset_fetcher.clone(),
            )
            .collect(),
        )
        .map(move |(hashes_generations, exclude_generations)| {
            Self::new_with_excludes_gen_num(
                ctx,
                &changeset_fetcher,
                lca_hint_index,
                hashes_generations,
                exclude_generations,
            )
        })
        .map_err(|err| err.context(ErrorKind::GenerationFetchFailed))
        .from_err()
        .flatten_stream()
        .boxify()
    }

    pub fn new_with_excludes_gen_num(
        ctx: CoreContext,
        changeset_fetcher: &ArcChangesetFetcher,
        lca_hint_index: Arc<dyn LeastCommonAncestorsHint>,
        hashes_generations: Vec<(ChangesetId, Generation)>,
        exclude_generations: Vec<(ChangesetId, Generation)>,
    ) -> BonsaiNodeStream {
        let mut next_generation = BTreeMap::new();
        let current_exclude_generation = exclude_generations
            .iter()
            .map(|(_node, gen)| gen)
            .max()
            .cloned();
        let mut sorted_unique_generations = UniqueHeap::new();
        for (hash, generation) in hashes_generations {
            next_generation
                .entry(generation.clone())
                .or_insert_with(HashSet::new)
                .insert(hash);
            // insert into our sorted list of generations
            sorted_unique_generations.push(generation);
        }

        Self {
            ctx,
            changeset_fetcher: changeset_fetcher.clone(),
            lca_hint_index,
            next_generation,
            // Start with a fake state - maximum generation number and no entries
            // for it (see drain below)
            current_generation: Generation::max_gen(),
            pending_changesets: SelectAll::default(),
            exclude_ancestors_future: ok(NodeFrontier::from_iter(exclude_generations)).boxify(),
            current_exclude_generation,
            drain: hashset! {}.into_iter().peekable(),
            sorted_unique_generations,
        }
        .boxify()
    }

    // Poll if a particular node should be excluded from the output.
    fn exclude_node(
        &mut self,
        node: ChangesetId,
        current_generation: Generation,
    ) -> Poll<bool, Error> {
        loop {
            // Poll the exclude_ancestors frontier future
            let curr_exclude_ancestors = try_ready!(self.exclude_ancestors_future.poll());

            if curr_exclude_ancestors.is_empty() {
                // No exclude nodes to worry about
                self.exclude_ancestors_future = ok(curr_exclude_ancestors).boxify();
                return Ok(Async::Ready(false));
            }

            if self.current_exclude_generation == None {
                // Recompute the current exclude generation
                self.current_exclude_generation = curr_exclude_ancestors.max_gen();
            }

            // Attempt to extract the max generation of the frontier
            if let Some(exclude_gen) = self.current_exclude_generation {
                match exclude_gen.cmp(&current_generation) {
                    Ordering::Less => {
                        self.exclude_ancestors_future = ok(curr_exclude_ancestors).boxify();
                        return Ok(Async::Ready(false));
                    }
                    Ordering::Equal => {
                        let mut should_exclude: Option<bool> = None;
                        {
                            if let Some(nodes) = curr_exclude_ancestors.get(&current_generation) {
                                should_exclude = Some(nodes.contains(&node));
                            }
                        }
                        if let Some(should_exclude) = should_exclude {
                            self.exclude_ancestors_future = ok(curr_exclude_ancestors).boxify();
                            return Ok(Async::Ready(should_exclude));
                        }
                    }
                    Ordering::Greater => {}
                }

                // Current generation in `exclude_ancestors` is bigger
                // than `current_generation`.
                // We need to skip.

                // Replace the exclude with a new future
                // And indicate the current exclude gen needs to be recalculated.
                self.current_exclude_generation = None;

                cloned!(self.lca_hint_index, self.ctx, self.changeset_fetcher);
                self.exclude_ancestors_future = async move {
                    lca_hint_index
                        .lca_hint(
                            &ctx,
                            &changeset_fetcher,
                            curr_exclude_ancestors,
                            current_generation,
                        )
                        .await
                }
                .boxed()
                .compat()
                .boxify();
            } else {
                // the max frontier is still "None".
                // So there are no nodes in our exclude frontier.
                self.exclude_ancestors_future = ok(curr_exclude_ancestors).boxify();
                return Ok(Async::Ready(false));
            }
        }
    }

    fn update_generation(&mut self) {
        let highest_generation = self
            .sorted_unique_generations
            .pop()
            .expect("Expected a non empty heap of generations");
        let new_generation = self
            .next_generation
            .remove(&highest_generation)
            .expect("Highest generation doesn't exist");
        self.current_generation = highest_generation;
        self.drain = new_generation.into_iter().peekable();
    }
}

impl Stream for DifferenceOfUnionsOfAncestorsNodeStream {
    type Item = ChangesetId;
    type Error = Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            // Empty the drain if any - return all items for this generation
            while self.drain.peek().is_some() {
                let current_generation = self.current_generation;

                let next_in_drain = *self.drain.peek().unwrap();
                if try_ready!(self.exclude_node(next_in_drain, current_generation)) {
                    self.drain.next();
                    continue;
                } else {
                    let next_in_drain = self.drain.next();
                    self.pending_changesets.push(make_pending(
                        self.ctx.clone(),
                        self.changeset_fetcher.clone(),
                        next_in_drain.unwrap(),
                    ));
                    return Ok(Async::Ready(next_in_drain));
                }
            }

            // Wait until we've drained pending_changesets - we can't continue until we
            // know about all parents of the just-output generation
            loop {
                match self.pending_changesets.poll()? {
                    Async::Ready(Some((hash, generation))) => {
                        self.next_generation
                            .entry(generation)
                            .or_insert_with(HashSet::new)
                            .insert(hash);
                        // insert into our sorted list of generations
                        self.sorted_unique_generations.push(generation);
                    }
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(None) => break,
                };
            }

            if self.next_generation.is_empty() {
                // All parents output - nothing more to send
                return Ok(Async::Ready(None));
            }

            self.update_generation();
        }
    }
}

#[cfg(test)]
mod test {
    use context::CoreContext;
    use fbinit::FacebookInit;
    use revset_test_helper::assert_changesets_sequence;
    use revset_test_helper::string_to_bonsai;
    use skiplist::SkiplistIndex;

    use super::*;
    use crate::fixtures::Linear;
    use crate::fixtures::MergeUneven;
    use crate::fixtures::TestRepoFixture;
    use crate::tests::TestChangesetFetcher;

    #[fbinit::test]
    async fn empty_ancestors_combinators(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let stream = DifferenceOfUnionsOfAncestorsNodeStream::new_union(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![],
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![], stream).await;

        let excludes =
            vec![string_to_bonsai(fb, &repo, "0ed509bf086fadcb8a8a5384dc3b550729b0fc17").await];

        let stream = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![],
            excludes,
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![], stream).await;
    }

    #[fbinit::test]
    async fn linear_ancestors_with_excludes(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let nodestream = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![string_to_bonsai(fb, &repo, "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157").await],
            vec![string_to_bonsai(fb, &repo, "0ed509bf086fadcb8a8a5384dc3b550729b0fc17").await],
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![string_to_bonsai(fb, &repo, "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157").await],
            nodestream,
        )
        .await;
    }

    #[fbinit::test]
    async fn linear_ancestors_with_excludes_empty(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let nodestream = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![string_to_bonsai(fb, &repo, "0ed509bf086fadcb8a8a5384dc3b550729b0fc17").await],
            vec![string_to_bonsai(fb, &repo, "0ed509bf086fadcb8a8a5384dc3b550729b0fc17").await],
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![], nodestream).await;
    }

    #[fbinit::test]
    async fn ancestors_union(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = MergeUneven::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let nodestream = DifferenceOfUnionsOfAncestorsNodeStream::new_union(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![
                string_to_bonsai(fb, &repo, "fc2cef43395ff3a7b28159007f63d6529d2f41ca").await,
                string_to_bonsai(fb, &repo, "16839021e338500b3cf7c9b871c8a07351697d68").await,
            ],
        )
        .boxify();
        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "fc2cef43395ff3a7b28159007f63d6529d2f41ca").await,
                string_to_bonsai(fb, &repo, "bc7b4d0f858c19e2474b03e442b8495fd7aeef33").await,
                string_to_bonsai(fb, &repo, "795b8133cf375f6d68d27c6c23db24cd5d0cd00f").await,
                string_to_bonsai(fb, &repo, "4f7f3fd428bec1a48f9314414b063c706d9c1aed").await,
                string_to_bonsai(fb, &repo, "16839021e338500b3cf7c9b871c8a07351697d68").await,
                string_to_bonsai(fb, &repo, "1d8a907f7b4bf50c6a09c16361e2205047ecc5e5").await,
                string_to_bonsai(fb, &repo, "b65231269f651cfe784fd1d97ef02a049a37b8a0").await,
                string_to_bonsai(fb, &repo, "d7542c9db7f4c77dab4b315edd328edf1514952f").await,
                string_to_bonsai(fb, &repo, "3cda5c78aa35f0f5b09780d971197b51cad4613a").await,
                string_to_bonsai(fb, &repo, "15c40d0abc36d47fb51c8eaec51ac7aad31f669c").await,
            ],
            nodestream,
        )
        .await;
    }

    #[fbinit::test]
    async fn merge_ancestors_from_merge_excludes(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = MergeUneven::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let nodestream = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![string_to_bonsai(fb, &repo, "d35b1875cdd1ed2c687e86f1604b9d7e989450cb").await],
            vec![
                string_to_bonsai(fb, &repo, "fc2cef43395ff3a7b28159007f63d6529d2f41ca").await,
                string_to_bonsai(fb, &repo, "16839021e338500b3cf7c9b871c8a07351697d68").await,
            ],
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "d35b1875cdd1ed2c687e86f1604b9d7e989450cb").await,
                string_to_bonsai(fb, &repo, "264f01429683b3dd8042cb3979e8bf37007118bc").await,
                string_to_bonsai(fb, &repo, "5d43888a3c972fe68c224f93d41b30e9f888df7c").await,
            ],
            nodestream,
        )
        .await;
    }

    #[fbinit::test]
    async fn merge_ancestors_from_merge_excludes_union(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = MergeUneven::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let nodestream = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
            ctx.clone(),
            &changeset_fetcher,
            Arc::new(SkiplistIndex::new()),
            vec![string_to_bonsai(fb, &repo, "d35b1875cdd1ed2c687e86f1604b9d7e989450cb").await],
            vec![string_to_bonsai(fb, &repo, "16839021e338500b3cf7c9b871c8a07351697d68").await],
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "d35b1875cdd1ed2c687e86f1604b9d7e989450cb").await,
                string_to_bonsai(fb, &repo, "264f01429683b3dd8042cb3979e8bf37007118bc").await,
                string_to_bonsai(fb, &repo, "5d43888a3c972fe68c224f93d41b30e9f888df7c").await,
                string_to_bonsai(fb, &repo, "fc2cef43395ff3a7b28159007f63d6529d2f41ca").await,
                string_to_bonsai(fb, &repo, "bc7b4d0f858c19e2474b03e442b8495fd7aeef33").await,
                string_to_bonsai(fb, &repo, "795b8133cf375f6d68d27c6c23db24cd5d0cd00f").await,
                string_to_bonsai(fb, &repo, "4f7f3fd428bec1a48f9314414b063c706d9c1aed").await,
                string_to_bonsai(fb, &repo, "b65231269f651cfe784fd1d97ef02a049a37b8a0").await,
                string_to_bonsai(fb, &repo, "d7542c9db7f4c77dab4b315edd328edf1514952f").await,
            ],
            nodestream,
        )
        .await;
    }
}
