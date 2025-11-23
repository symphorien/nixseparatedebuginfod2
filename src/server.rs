// SPDX-FileCopyrightText: 2023 Guillaume Girol <symphorien+git@xlumurb.eu>
//
// SPDX-License-Identifier: GPL-3.0-only

//! An http server serving what [Debuginfod] can fetch.
//!
//! References:
//! Protocol: <https://www.mankier.com/8/debuginfod#Webapi>

use anyhow::Context;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use futures::StreamExt as _;
use http::header::{HeaderMap, CONTENT_LENGTH};
use std::fmt::Debug;
use std::future::IntoFuture as _;
use std::os::unix::prelude::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::io::ReaderStream;

use crate::build_id::BuildId;
use crate::debuginfod::Debuginfod;
use crate::substituter::multiplex::MultiplexingSubstituter;
use crate::vfs::AsFile;
use crate::Options;

#[derive(Clone)]
struct ServerState {
    debuginfod: Arc<Debuginfod>,
}

/// Serve the content of this file, or an appropriate error.
///
/// If the file is None, serve 404 not found.
async fn unwrap_file<T: AsFile + Debug>(
    path: anyhow::Result<Option<T>>,
) -> Result<(HeaderMap, Body), (StatusCode, String)> {
    let response = match path {
        Ok(Some(ref p)) => {
            match p.open().await {
                Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e))),
                Ok(file) => {
                    let mut headers = HeaderMap::new();
                    if let Ok(metadata) = file.metadata().await {
                        if let Ok(value) = metadata.size().to_string().parse() {
                            headers.insert(CONTENT_LENGTH, value);
                        }
                    }
                    tracing::info!("returning {:?}", &path);
                    // convert the `AsyncRead` into a `Stream`
                    let stream = ReaderStream::new(file);
                    // convert the `Stream` into an `axum::body::HttpBody`
                    let body = Body::from_stream(stream);
                    Ok((headers, body))
                }
            }
        }
        Ok(None) => Err((StatusCode::NOT_FOUND, "not found in cache".to_string())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e))),
    };
    if let Err((code, error)) = &response {
        tracing::info!("Responding error {}: {}", code, error);
    };
    response
}

fn validate_build_id(raw: &str) -> Result<BuildId, (StatusCode, String)> {
    match BuildId::new(raw) {
        Ok(b) => Ok(b),
        Err(e) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("parsing build_id in query path: {:#}", e),
        )),
    }
}

#[axum_macros::debug_handler]
async fn get_debuginfo(
    Path(build_id): Path<String>,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    let build_id = validate_build_id(&build_id)?;
    let res = assert_send(state.debuginfod.debuginfo(&build_id)).await;
    unwrap_file(res).await
}

#[axum_macros::debug_handler]
async fn get_executable(
    Path(build_id): Path<String>,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    let build_id = validate_build_id(&build_id)?;
    let res = assert_send(state.debuginfod.executable(&build_id)).await;
    unwrap_file(res).await
}

#[axum_macros::debug_handler]
async fn get_source(
    Path((build_id, request)): Path<(String, String)>,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    let build_id = validate_build_id(&build_id)?;
    let res = state.debuginfod.source(&build_id, &request).await;
    unwrap_file(res).await
}

async fn get_section(Path(_param): Path<(String, String)>) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED
}

fn assert_send<'a, T, U: std::future::Future<Output = T> + Send + 'a>(fut: U) -> U {
    fut
}

/// Starts the server according to command line arguments contained in `args`.
///
/// Does not actually return.
pub async fn run_server(args: Options) -> anyhow::Result<()> {
    let substituter = MultiplexingSubstituter::new_from_urls(args.substituter.iter()).await?;
    let state = ServerState {
        debuginfod: Arc::new(
            Debuginfod::new(
                PathBuf::from(&args.cache_dir),
                Box::new(substituter),
                args.expiration,
            )
            .await?,
        ),
    };
    state.debuginfod.spawn_cleanup_task();
    let app = Router::new()
        .route("/buildid/{buildid}/section/{section}", get(get_section))
        .route("/buildid/{buildid}/source/{*path}", get(get_source))
        .route("/buildid/{buildid}/executable", get(get_executable))
        .route("/buildid/{buildid}/debuginfo", get(get_debuginfo))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);
    let listeners = match args.listen_address {
        Some(addr) => vec![tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("opening listen socket on {}", addr))?],
        None => {
            let fds = systemd::daemon::listen_fds(false)
                .context("listing socket activation file descriptors")?;
            let mut listeners = vec![];
            for fd in fds.iter() {
                let std_listener = systemd::daemon::tcp_listener(fd)
                    .with_context(|| format!("socket activation yielded bad fd {fd}"))?;
                std_listener.set_nonblocking(true).with_context(|| {
                    format!("failed to set socket activation fd {fd} non blocking")
                })?;
                let listener = tokio::net::TcpListener::from_std(std_listener)
                    .with_context(|| format!("socket activation yielded bad fd {fd} for async"))?;
                listeners.push(listener);
            }
            listeners
        }
    };
    anyhow::ensure!(!listeners.is_empty(), "no listen address was specified with --listen-address and systemd socket activation was not used");
    for l in listeners.iter() {
        match l.local_addr() {
            Ok(a) => tracing::info!("listening on {a}"),
            Err(e) => tracing::warn!("listening on unknown address: {e}"),
        };
    }
    let mut server: futures::stream::FuturesUnordered<_> = listeners
        .into_iter()
        .map(|l| axum::serve::serve(l, app.clone().into_make_service()).into_future())
        .collect();
    if let Err(e) = systemd::daemon::notify(false, [(systemd::daemon::STATE_READY, "1")].iter()) {
        tracing::warn!("failed to notify systemd READY=1: {e}");
    }
    let mut last_err = Ok(());
    while let Some(result) = server.next().await {
        if let Err(e) = result {
            tracing::error!("failed to serve: {e}");
            last_err = Err(e).context("running server");
        }
    }
    last_err
}
