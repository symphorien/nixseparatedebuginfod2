use std::path::Path;

use anyhow::Context;
use reqwest::Url;
use tracing::Instrument;

use crate::{
    build_id::BuildId, store_path::StorePath, utils::percent_encode_to_filename,
    vfs::RestrictedPath,
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
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let mut result = Ok(None);
        for substituter in self.substituters.iter() {
            let span =
                tracing::trace_span!("inside MultiplexingSubstituter", substituter=?substituter);
            tracing::trace!(parent: &span, "querying inner substituter");
            match substituter
                .build_id_to_debug_output(build_id)
                .instrument(span.clone())
                .await
            {
                Ok(Some(p)) => {
                    tracing::trace!(parent: &span, "substituter has the requested debug output");
                    return Ok(Some(p));
                }
                Ok(None) => {
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
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let mut result = Ok(None);
        for substituter in self.substituters.iter() {
            let span = tracing::trace_span!("querying inside MultiplexingSubstituter", substituter=?substituter);
            tracing::trace!(parent: &span, "querying inner substituter");
            match substituter
                .fetch_store_path(store_path)
                .instrument(span.clone())
                .await
            {
                Ok(Some(p)) => {
                    tracing::trace!(parent: &span, "substituter has the request store path");
                    return Ok(Some(p));
                }
                Ok(None) => {
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

    fn spawn_cleanup_task(&self) {
        for substituter in self.substituters.iter() {
            substituter.spawn_cleanup_task()
        }
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
    pub async fn new_from_urls<'a, I: Iterator<Item = &'a Url>>(
        urls: I,
        cache_dir: &Path,
        expiration: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let mut substituters = vec![];
        for url in urls {
            let dirname = percent_encode_to_filename(url.as_str());
            let d = cache_dir.join(dirname);
            tokio::fs::create_dir_all(&d)
                .await
                .with_context(|| format!("mkdir({d:?})"))?;
            let substituter = substituter_from_url(url, d, expiration).await?;
            substituters.push(substituter);
        }
        Ok(Self::new(substituters.into_iter()))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Deref,
        sync::{
            atomic::{AtomicU32, Ordering},
            Arc,
        },
    };

    use tempfile::TempDir;

    use crate::{utils::Presence, vfs::ResolvedPathKind};

    use super::*;
    #[derive(Debug)]
    struct MockSubstituter {
        answer: Result<Presence, String>,
        priority: Priority,
        call_count: AtomicU32,
        out_dir: TempDir,
    }

    impl MockSubstituter {
        fn new(answer: Result<Presence, String>, priority: Priority) -> Self {
            Self {
                answer,
                priority,
                call_count: AtomicU32::new(0),
                out_dir: TempDir::new().unwrap(),
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
            build_id: &BuildId,
        ) -> anyhow::Result<Option<RestrictedPath>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Err(ref e) => Err(anyhow::anyhow!(
                    "MockSubstituter failed in build_id_to_debug_output: {e}"
                )),
                Ok(Presence::NotFound) => Ok(None),
                Ok(Presence::Found) => {
                    let dir = self.out_dir.path().join(build_id.deref());
                    tokio::fs::create_dir_all(&dir).await.unwrap();
                    RestrictedPath::new(dir, None).await.map(Some)
                }
            }
        }
        async fn fetch_store_path(
            &self,
            store_path: &StorePath,
        ) -> anyhow::Result<Option<RestrictedPath>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Err(ref e) => Err(anyhow::anyhow!(
                    "MockSubstituter failed in fetch_store_path: {e}"
                )),
                Ok(Presence::NotFound) => Ok(None),
                Ok(Presence::Found) => {
                    let dir = self.out_dir.path().join(store_path.hash());
                    tokio::fs::create_dir_all(&dir).await.unwrap();
                    RestrictedPath::new(dir, None).await.map(Some)
                }
            }
        }

        fn priority(&self) -> Priority {
            self.priority
        }

        fn spawn_cleanup_task(&self) {}
    }

    #[tokio::test]
    async fn nominal() {
        // two substituters have the requested resource, only the most local one is queried.
        let sub1 = Arc::new(MockSubstituter::new(Ok(Presence::Found), Priority::Remote));
        let sub2 = Arc::new(MockSubstituter::new(Ok(Presence::Found), Priority::Local));
        let subs: [BoxedSubstituter; 2] = [Box::new(sub1.clone()), Box::new(sub2.clone())];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let out = sub
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json",
                ))
                .unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 0);
        // check that it exists
        assert_eq!(
            out.resolve_inside_root()
                .await
                .unwrap()
                .unwrap()
                .kind()
                .await
                .unwrap(),
            ResolvedPathKind::Directory
        );

        let out2 = sub
            .build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 0);
        assert_eq!(
            out2.resolve_inside_root()
                .await
                .unwrap()
                .unwrap()
                .kind()
                .await
                .unwrap(),
            ResolvedPathKind::Directory
        );
    }

    #[tokio::test]
    async fn error_then_success() {
        // first substituter does not have the resource, second errors, last has it. No error is
        // reported because the resource was found in the end.
        let sub0 = Arc::new(MockSubstituter::new(Ok(Presence::Found), Priority::Remote));
        let sub1 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            Priority::Local,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Err("ahah".into()),
            Priority::LocalUnpacked,
        ));
        let subs: [BoxedSubstituter; 3] = [
            Box::new(sub0.clone()),
            Box::new(sub1.clone()),
            Box::new(sub2.clone()),
        ];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let out = sub
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json",
                ))
                .unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 1);
        assert_eq!(sub0.call_count(), 1);
        assert_eq!(
            out.resolve_inside_root()
                .await
                .unwrap()
                .unwrap()
                .kind()
                .await
                .unwrap(),
            ResolvedPathKind::Directory
        );

        let out2 = sub
            .build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 2);
        assert_eq!(sub0.call_count(), 2);
        assert_eq!(
            out2.resolve_inside_root()
                .await
                .unwrap()
                .unwrap()
                .kind()
                .await
                .unwrap(),
            ResolvedPathKind::Directory
        );
    }

    #[tokio::test]
    async fn unrecoverable_error() {
        // first sub is error, second is error, last does not have the resource. the second error
        // is returned.
        let sub1 = Arc::new(MockSubstituter::new(
            Err("first error".into()),
            Priority::Unknown,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Err("second error".into()),
            Priority::Unknown,
        ));
        let sub3 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            Priority::Unknown,
        ));
        let subs: [BoxedSubstituter; 3] = [
            Box::new(sub1.clone()),
            Box::new(sub2.clone()),
            Box::new(sub3.clone()),
        ];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        let err = dbg!(sub
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
            )
            .await
            .unwrap_err()
            .to_string());
        assert!(err.contains("second error"));
        assert_eq!(sub1.call_count(), 1);
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub3.call_count(), 1);

        assert!(dbg!(sub
            .build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
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
            Priority::Remote,
        ));
        let sub2 = Arc::new(MockSubstituter::new(
            Ok(Presence::NotFound),
            Priority::Local,
        ));
        let subs: [BoxedSubstituter; 2] = [Box::new(sub1.clone()), Box::new(sub2.clone())];
        let sub = MultiplexingSubstituter::new(subs.into_iter());
        assert!(sub
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/ab10xdj7v3hsa0j4lvj4zdadzg4n12nn-boot.json"
                ))
                .unwrap(),
            )
            .await
            .unwrap()
            .is_none(),);
        assert_eq!(sub2.call_count(), 1);
        assert_eq!(sub1.call_count(), 1);

        assert!(sub
            .build_id_to_debug_output(
                &BuildId::new("b91c254ef8c76310683ce217f6269bc2f3e84d65").unwrap(),
            )
            .await
            .unwrap()
            .is_none(),);
        assert_eq!(sub2.call_count(), 2);
        assert_eq!(sub1.call_count(), 2);
    }
}
