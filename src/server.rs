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
use http::header::{HeaderMap, CONTENT_LENGTH};
use std::fmt::Debug;
use std::os::unix::prelude::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::io::ReaderStream;

use crate::build_id::BuildId;
use crate::debuginfod::Debuginfod;
use crate::substituter::substituter_from_url;
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
    let substituter = substituter_from_url(&args.substituter).await?;
    let state = ServerState {
        debuginfod: Arc::new(
            Debuginfod::new(PathBuf::from(&args.cache_dir), substituter, args.expiration).await?,
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
    let listener = tokio::net::TcpListener::bind(&args.listen_address)
        .await
        .with_context(|| format!("opening listen socket on {}", &args.listen_address))?;
    axum::serve::serve(listener, app.into_make_service()).await?;
    Ok(())
}
