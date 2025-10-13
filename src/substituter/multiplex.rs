use std::path::Path;

use reqwest::Url;
use tracing::Instrument;

use crate::{
    build_id::BuildId,
    store_path::StorePath,
    utils::{remove_recursively_if_exists, Presence},
};

use super::{substituter_from_url, BoxedSubstituter, Priority, Substituter};

#[derive(Debug)]
/// A substituter which tries its constituent substituters in succession until one succeeds
pub struct MultiplexingSubstituter {
    substituters: Vec<BoxedSubstituter>,
}

#[async_trait::async_trait]
impl Substituter for MultiplexingSubstituter {
    #[tracing::instrument]
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        let mut result = Ok(Presence::NotFound);
        for substituter in self.substituters.iter() {
            let span =
                tracing::trace_span!("inside MultiplexingSubstituter", substituter=?substituter);
            remove_recursively_if_exists(into)
                .instrument(span.clone())
                .await?;
            tracing::trace!(parent: &span, "querying inner substituter");
            match substituter
                .build_id_to_debug_output(build_id, into)
                .instrument(span.clone())
                .await
            {
                Ok(Presence::Found) => {
                    tracing::trace!(parent: &span, "substituter has the requested debug output");
                    return Ok(Presence::Found);
                }
                Ok(Presence::NotFound) => {
                    tracing::trace!(parent: &span, "substituter does not have the requested debug output")
                }
                Err(e) => {
                    tracing::trace!(parent: &span, "substituter failed: {e:#}");
                    result = Err(e);
                }
            }
        }
        result
    }

    #[tracing::instrument]
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        let mut result = Ok(Presence::NotFound);
        for substituter in self.substituters.iter() {
            let span = tracing::trace_span!("querying inside MultiplexingSubstituter", substituter=?substituter);
            remove_recursively_if_exists(into)
                .instrument(span.clone())
                .await?;
            tracing::trace!(parent: &span, "querying inner substituter");
            match substituter
                .fetch_store_path(store_path, into)
                .instrument(span.clone())
                .await
            {
                Ok(Presence::Found) => {
                    tracing::trace!(parent: &span, "substituter has the request store path");
                    return Ok(Presence::Found);
                }
                Ok(Presence::NotFound) => {
                    tracing::trace!(parent: &span, "substituter does not have requested store_path")
                }
                Err(e) => {
                    tracing::trace!(parent: &span, "substituter failed: {e:#}");
                    result = Err(e);
                }
            }
        }
        result
    }

    fn priority(&self) -> Priority {
        Priority::Unknown
    }
}

impl MultiplexingSubstituter {
    /// Creates a new substituers that contains the union of all the NARs of the provided
    /// substituters.
    ///
    /// substituters are tried sequentially in they priority order
    pub fn new<I: Iterator<Item = BoxedSubstituter>>(substituers: I) -> Self {
        let mut result = Self {
            substituters: substituers.collect(),
        };
        result.substituters.sort_by_key(|s| s.priority());
        result
    }

    /// Same as [MultiplexingSubstituter::new] but constructs substituters from Urls instead.
    ///
    /// See [substituter_from_url] for details.
    pub async fn new_from_urls<'a, I: Iterator<Item = &'a Url>>(urls: I) -> anyhow::Result<Self> {
        let mut substituters = vec![];
        for url in urls {
            let substituter = substituter_from_url(url).await?;
            substituters.push(substituter);
        }
        Ok(Self::new(substituters.into_iter()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    use super::*;
    #[derive(Debug)]
    struct MockSubstituter {
        answer: Result<Presence, String>,
        /// if true, querying the substituter create the target dir
        side_effect: bool,
        priority: Priority,
        call_count: AtomicU32,
    }

    impl MockSubstituter {
        fn new(answer: Result<Presence, String>, side_effect: bool, priority: Priority) -> Self {
            Self {
                answer,
                side_effect,
                priority,
                call_count: AtomicU32::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            self.call_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Substituter for MockSubstituter {
        async fn build_id_to_debug_output(
            &self,
            _build_id: &BuildId,
            into: &Path,
        ) -> anyhow::Result<Presence> {
            if self.side_effect {
                tokio::fs::create_dir(into).await.unwrap();
                tokio::fs::write(into.join("file"), "content")
                    .await
                    .unwrap();
            }
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.answer.clone().map_err(|s| {
                anyhow::anyhow!("MockSubstituter failed in build_id_to_debug_output: {s}")
            })
        }
        async fn fetch_store_path(
            &self,
            _store_path: &StorePath,
            into: &Path,
        ) -> anyhow::Result<Presence> {
            if self.side_effect {
                tokio::fs::create_dir(into).await.unwrap();
                tokio::fs::write(into.join("file"), "content")
                    .await
                    .unwrap();
            }
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.answer
                .clone()
                .map_err(|s| anyhow::anyhow!("MockSubstituter failed in fetch_store_path: {s}"))
        }

        fn priority(&self) -> Priority {
            self.priority
        }
    }

    #[tokio::test]
    async fn nominal() {
        // two substituters have the requested resource, only the most local one is queried.
        let sub1 = Arc::new(MockSubstituter::new(
            Ok(Presence::Found),
            true,
            Priority::Remote,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Ok(Presence::Found),
            true,
            Priority::Local,
        ));
        let subs: [BoxedSubstituter; 2] = [Box::new(sub1.clone()), Box::new(sub2.clone())];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("into");
        assert_eq!(
            sub.fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap(),
            Presence::Found
        );
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 0);
        assert!(into.exists());

        let into2 = dir.path().join("into2");
        assert_eq!(
            sub.build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
                &into2
            )
            .await
            .unwrap(),
            Presence::Found
        );
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 0);
        assert!(into2.exists());
    }

    #[tokio::test]
    async fn error_then_success() {
        // first substituter does not have the resource, second errors, last has it. No error is
        // reported because the resource was found in the end.
        let sub0 = Arc::new(MockSubstituter::new(
            Ok(Presence::Found),
            true,
            Priority::Remote,
        ));
        let sub1 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            false,
            Priority::Local,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Err("ahah".into()),
            true,
            Priority::LocalUnpacked,
        ));
        let subs: [BoxedSubstituter; 3] = [
            Box::new(sub0.clone()),
            Box::new(sub1.clone()),
            Box::new(sub2.clone()),
        ];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("into");
        assert_eq!(
            sub.fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap(),
            Presence::Found
        );
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 1);
        assert_eq!(sub0.call_count(), 1);
        assert!(into.exists());

        let into2 = dir.path().join("into2");
        assert_eq!(
            sub.build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
                &into2
            )
            .await
            .unwrap(),
            Presence::Found
        );
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 2);
        assert_eq!(sub0.call_count(), 2);
        assert!(into2.exists());
    }

    #[tokio::test]
    async fn unrecoverable_error() {
        // first sub is error, second is error, last does not have the resource. the second error
        // is returned.
        let sub1 = Arc::new(MockSubstituter::new(
            Err("first error".into()),
            true,
            Priority::Unknown,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Err("second error".into()),
            true,
            Priority::Unknown,
        ));
        let sub3 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            false,
            Priority::Unknown,
        ));
        let subs: [BoxedSubstituter; 3] = [
            Box::new(sub1.clone()),
            Box::new(sub2.clone()),
            Box::new(sub3.clone()),
        ];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("into");
        assert!(dbg!(sub
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap_err()
            .to_string())
        .contains("second error"));
        assert_eq!(sub1.call_count(), 1);
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub3.call_count(), 1);

        let into2 = dir.path().join("into2");
        assert!(dbg!(sub
            .build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
                &into2
            )
            .await
            .unwrap_err()
            .to_string())
        .contains("second error"));
        assert_eq!(sub1.call_count(), 2);
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub3.call_count(), 2);
    }

    #[tokio::test]
    async fn not_found() {
        // no substituters have the requested resource
        let sub1 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            false,
            Priority::Remote,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            false,
            Priority::Local,
        ));
        let subs: [BoxedSubstituter; 2] = [Box::new(sub1.clone()), Box::new(sub2.clone())];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("into");
        assert_eq!(
            sub.fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap(),
            Presence::NotFound
        );
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 1);

        let into2 = dir.path().join("into2");
        assert_eq!(
            sub.build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
                &into2
            )
            .await
            .unwrap(),
            Presence::NotFound
        );
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 2);
    }
}
