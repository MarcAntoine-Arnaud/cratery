/*******************************************************************************
 * Copyright (c) 2024 Cénotélie Opérations SAS (cenotelie.fr)
 ******************************************************************************/

//! Implementation of axum routes to expose the application

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header::{HeaderName, SET_COOKIE};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::{BoxError, Json};
use cookie::Key;
use futures::Stream;
use serde::Deserialize;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::application::Application;
use crate::model::auth::{AuthenticatedUser, RegistryUserToken, RegistryUserTokenWithSecret};
use crate::model::cargo::{
    CrateUploadResult, OwnersChangeQuery, OwnersQueryResult, RegistryUser, SearchResults, YesNoMsgResult, YesNoResult,
};
use crate::model::deps::DepsAnalysis;
use crate::model::packages::CrateInfo;
use crate::model::stats::{DownloadStats, GlobalStats};
use crate::model::{generate_token, AppVersion, CrateAndVersion};
use crate::services::index::Index;
use crate::utils::apierror::{error_invalid_request, error_not_found, specialize, ApiError};
use crate::utils::axum::auth::{AuthData, AxumStateForCookies};
use crate::utils::axum::embedded::Resources;
use crate::utils::axum::extractors::Base64;
use crate::utils::axum::{response, response_error, ApiResult};

/// The state of this application for axum
pub struct AxumState {
    /// The main application
    pub application: Arc<Application>,
    /// Key to access private cookies
    pub cookie_key: Key,
    /// The static resources for the web app
    pub webapp_resources: Resources,
}

impl AxumStateForCookies for AxumState {
    fn get_domain(&self) -> Cow<'static, str> {
        Cow::Owned(self.application.configuration.web_domain.clone())
    }

    fn get_id_cookie_name(&self) -> Cow<'static, str> {
        Cow::Borrowed("cratery-user")
    }

    fn get_cookie_key(&self) -> &Key {
        &self.cookie_key
    }
}

#[derive(Deserialize)]
pub struct PathInfoCrate {
    package: String,
}

#[derive(Deserialize)]
pub struct PathInfoCrateVersion {
    package: String,
    version: String,
}

/// Response for a GET on the root
/// Redirect to the web app
pub async fn get_root(State(state): State<Arc<AxumState>>) -> (StatusCode, [(HeaderName, HeaderValue); 2]) {
    let target = format!("{}/webapp/index.html", state.application.configuration.web_public_uri);
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
    )
}

/// Gets the favicon
pub async fn get_favicon(State(state): State<Arc<AxumState>>) -> (StatusCode, [(HeaderName, HeaderValue); 2], &'static [u8]) {
    let favicon = state.webapp_resources.get("favicon.png").unwrap();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(favicon.content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
        ],
        favicon.content,
    )
}

/// Gets the redirection response when not authenticated
fn get_auth_redirect(state: &AxumState) -> (StatusCode, [(HeaderName, HeaderValue); 2]) {
    // redirect to login
    let nonce = generate_token(64);
    let oauth_state = generate_token(32);
    let target = format!(
        "{}?response_type={}&redirect_uri={}&client_id={}&scope={}&nonce={}&state={}",
        state.application.configuration.oauth_login_uri,
        "code",
        urlencoding::encode(&format!(
            "{}/webapp/oauthcallback.html",
            state.application.configuration.web_public_uri
        )),
        urlencoding::encode(&state.application.configuration.oauth_client_id),
        urlencoding::encode(&state.application.configuration.oauth_client_scope),
        nonce,
        oauth_state
    );
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
    )
}

/// Gets the redirection for a crates shortcut
pub async fn get_redirection_crate(
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> (StatusCode, [(HeaderName, HeaderValue); 2]) {
    let target = format!("/webapp/crate.html?crate={package}");
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
            (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
        ],
    )
}

/// Gets the redirection for a crates shortcut
pub async fn get_redirection_crate_version(
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> (StatusCode, [(HeaderName, HeaderValue); 2]) {
    let target = format!("/webapp/crate.html?crate={package}&version={version}");
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
            (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
        ],
    )
}

/// Gets the favicon
pub async fn get_webapp_resource(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    request: Request<Body>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 2], &'static [u8]), StatusCode> {
    let path = request.uri().path();
    let path = &path["/webapp/".len()..];

    if let Some(crate_name) = path.strip_prefix("crates/") {
        // URL shortcut for crates
        let target = format!("/webapp/crate.html?crate={crate_name}");
        return Ok((
            StatusCode::FOUND,
            [
                (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
                (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
            ],
            &[],
        ));
    }

    if path == "index.html" {
        let is_authenticated = state.application.authenticate(&auth_data).await.is_ok();
        if !is_authenticated {
            let (code, headers) = get_auth_redirect(&state);
            return Ok((code, headers, &[]));
        }
    }

    let resource = state.webapp_resources.get(path);
    match resource {
        Some(resource) => Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static(resource.content_type)),
                (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
            ],
            resource.content,
        )),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Redirects to the login page
pub async fn webapp_me(State(state): State<Arc<AxumState>>) -> (StatusCode, [(HeaderName, HeaderValue); 2]) {
    let target = format!("{}/webapp/index.html", state.application.configuration.web_public_uri);
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, HeaderValue::from_str(&target).unwrap()),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
    )
}

/// Gets a file from the documentation
pub async fn get_docs_resource(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    request: Request<Body>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 2], Body), (StatusCode, [(HeaderName, HeaderValue); 1], Body)> {
    let is_authenticated = state.application.authenticate(&auth_data).await.is_ok();
    if !is_authenticated {
        let (code, headers) = get_auth_redirect(&state);
        return Ok((code, headers, Body::empty()));
    }

    let path = &request.uri().path()[1..]; // strip leading /
    assert!(path.starts_with("docs/"));
    let extension = get_content_type(path);
    match state.application.get_service_storage().download_doc_file(&path[5..]).await {
        Ok(content) => Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_str(extension).unwrap()),
                (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600")),
            ],
            Body::from(content),
        )),
        Err(e) => {
            let message = e.to_string();
            Err((
                StatusCode::NOT_FOUND,
                [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
                Body::from(message),
            ))
        }
    }
}

fn get_content_type(name: &str) -> &'static str {
    let extension = name.rfind('.').map(|index| &name[(index + 1)..]);
    match extension {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("gif") => "image/gif",
        Some("png") => "image/png",
        Some("jpeg") => "image/jpeg",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Get the current user
pub async fn api_v1_get_current_user(auth_data: AuthData, State(state): State<Arc<AxumState>>) -> ApiResult<RegistryUser> {
    response(state.application.get_current_user(&auth_data).await)
}

/// Attempts to login using an OAuth code
pub async fn api_v1_login_with_oauth_code(
    mut auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    body: Bytes,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 1], Json<RegistryUser>), (StatusCode, Json<ApiError>)> {
    let code = String::from_utf8_lossy(&body);
    let registry_user = state.application.login_with_oauth_code(&code).await.map_err(response_error)?;
    let cookie = auth_data.create_id_cookie(&AuthenticatedUser {
        uid: registry_user.id,
        principal: registry_user.email.clone(),
        // when authenticated via cookies, can do everything
        can_write: true,
        can_admin: true,
    });
    Ok((
        StatusCode::OK,
        [(SET_COOKIE, HeaderValue::from_str(&cookie.to_string()).unwrap())],
        Json(registry_user),
    ))
}

/// Logout a user
pub async fn api_v1_logout(mut auth_data: AuthData) -> (StatusCode, [(HeaderName, HeaderValue); 1]) {
    let cookie = auth_data.create_expired_id_cookie();
    (
        StatusCode::OK,
        [(SET_COOKIE, HeaderValue::from_str(&cookie.to_string()).unwrap())],
    )
}

/// Gets the tokens for a user
pub async fn api_v1_get_tokens(auth_data: AuthData, State(state): State<Arc<AxumState>>) -> ApiResult<Vec<RegistryUserToken>> {
    response(state.application.get_tokens(&auth_data).await)
}

#[derive(Deserialize)]
pub struct CreateTokenQuery {
    #[serde(rename = "canWrite")]
    can_write: bool,
    #[serde(rename = "canAdmin")]
    can_admin: bool,
}

/// Creates a token for the current user
pub async fn api_v1_create_token(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Query(CreateTokenQuery { can_write, can_admin }): Query<CreateTokenQuery>,
    name: String,
) -> ApiResult<RegistryUserTokenWithSecret> {
    response(state.application.create_token(&auth_data, &name, can_write, can_admin).await)
}

/// Revoke a previous token
pub async fn api_v1_revoke_token(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(token_id): Path<i64>,
) -> ApiResult<()> {
    response(state.application.revoke_token(&auth_data, token_id).await)
}

/// Gets the known users
pub async fn api_v1_get_users(auth_data: AuthData, State(state): State<Arc<AxumState>>) -> ApiResult<Vec<RegistryUser>> {
    response(state.application.get_users(&auth_data).await)
}

/// Updates the information of a user
pub async fn api_v1_update_user(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(Base64(email)): Path<Base64>,
    target: Json<RegistryUser>,
) -> ApiResult<RegistryUser> {
    if email != target.email {
        return Err(response_error(specialize(
            error_invalid_request(),
            String::from("email in path and body are different"),
        )));
    }
    response(state.application.update_user(&auth_data, &target).await)
}

/// Attempts to delete a user
pub async fn api_v1_delete_user(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(Base64(email)): Path<Base64>,
) -> ApiResult<()> {
    response(state.application.delete_user(&auth_data, &email).await)
}

/// Attempts to deactivate a user
pub async fn api_v1_deactivate_user(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(Base64(email)): Path<Base64>,
) -> ApiResult<()> {
    response(state.application.deactivate_user(&auth_data, &email).await)
}

/// Attempts to deactivate a user
pub async fn api_v1_reactivate_user(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(Base64(email)): Path<Base64>,
) -> ApiResult<()> {
    response(state.application.reactivate_user(&auth_data, &email).await)
}

#[derive(Deserialize)]
pub struct SearchForm {
    q: String,
    per_page: Option<usize>,
}

pub async fn api_v1_cargo_search(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    form: Query<SearchForm>,
) -> ApiResult<SearchResults> {
    response(state.application.search_crates(&auth_data, &form.q, form.per_page).await)
}

/// Gets the global statistics for the registry
pub async fn api_v1_get_crates_stats(auth_data: AuthData, State(state): State<Arc<AxumState>>) -> ApiResult<GlobalStats> {
    response(state.application.get_crates_stats(&auth_data).await)
}

/// Gets all the packages that are outdated while also being the latest version
pub async fn api_v1_get_crates_outdated_heads(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
) -> ApiResult<Vec<CrateAndVersion>> {
    response(state.application.get_crates_outdated_heads(&auth_data).await)
}

pub async fn api_v1_cargo_publish_crate_version(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    body: Bytes,
) -> ApiResult<CrateUploadResult> {
    response(state.application.publish_crate_version(&auth_data, &body).await)
}

pub async fn api_v1_get_crate_info(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> ApiResult<CrateInfo> {
    response(state.application.get_crate_info(&auth_data, &package).await)
}

pub async fn api_v1_get_crate_last_readme(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 1], Vec<u8>), (StatusCode, Json<ApiError>)> {
    let data = state
        .application
        .get_crate_last_readme(&auth_data, &package)
        .await
        .map_err(response_error)?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/markdown"))],
        data,
    ))
}

pub async fn api_v1_get_crate_readme(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 1], Vec<u8>), (StatusCode, Json<ApiError>)> {
    let data = state
        .application
        .get_crate_readme(&auth_data, &package, &version)
        .await
        .map_err(response_error)?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/markdown"))],
        data,
    ))
}

pub async fn api_v1_download_crate(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 1], Vec<u8>), (StatusCode, Json<ApiError>)> {
    match state.application.get_crate_content(&auth_data, &package, &version).await {
        Ok(data) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))],
            data,
        )),
        Err(mut error) => {
            if error.http == 401 {
                // map to 403
                error.http = 403;
            }
            Err(response_error(error))
        }
    }
}

pub async fn api_v1_cargo_yank(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> ApiResult<YesNoResult> {
    response(state.application.yank_crate_version(&auth_data, &package, &version).await)
}

pub async fn api_v1_cargo_unyank(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> ApiResult<YesNoResult> {
    response(state.application.unyank_crate_version(&auth_data, &package, &version).await)
}

pub async fn api_v1_regen_crate_version_doc(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> ApiResult<()> {
    response(
        state
            .application
            .regen_crate_version_doc(&auth_data, &package, &version)
            .await,
    )
}

pub async fn api_v1_check_crate_version(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrateVersion { package, version }): Path<PathInfoCrateVersion>,
) -> ApiResult<DepsAnalysis> {
    response(
        state
            .application
            .check_crate_version_deps(&auth_data, &package, &version)
            .await,
    )
}

/// Gets the download statistics for a crate
pub async fn api_v1_get_crate_dl_stats(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> ApiResult<DownloadStats> {
    response(state.application.get_crate_dl_stats(&auth_data, &package).await)
}

pub async fn api_v1_cargo_get_crate_owners(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> ApiResult<OwnersQueryResult> {
    response(state.application.get_crate_owners(&auth_data, &package).await)
}

pub async fn api_v1_cargo_add_crate_owners(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
    input: Json<OwnersChangeQuery>,
) -> ApiResult<YesNoMsgResult> {
    response(state.application.add_crate_owners(&auth_data, &package, &input.users).await)
}

pub async fn api_v1_cargo_remove_crate_owners(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
    input: Json<OwnersChangeQuery>,
) -> ApiResult<YesNoResult> {
    response(
        state
            .application
            .remove_crate_owners(&auth_data, &package, &input.users)
            .await,
    )
}

/// Gets the targets for a crate
pub async fn api_v1_get_crate_targets(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
) -> ApiResult<Vec<String>> {
    response(state.application.get_crate_targets(&auth_data, &package).await)
}

/// Sets the targets for a crate
pub async fn api_v1_set_crate_targets(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Path(PathInfoCrate { package }): Path<PathInfoCrate>,
    input: Json<Vec<String>>,
) -> ApiResult<()> {
    response(state.application.set_crate_targets(&auth_data, &package, &input).await)
}

pub async fn index_serve_inner(
    index: &Index,
    path: &str,
) -> Result<(impl Stream<Item = Result<impl Into<Bytes>, impl Into<BoxError>>>, HeaderValue), ApiError> {
    let file_path: PathBuf = path.parse()?;
    let file_path = index.get_index_file(&file_path).ok_or_else(error_not_found)?;
    let file = File::open(file_path).await.map_err(|_e| error_not_found())?;
    let stream = ReaderStream::new(file);
    if std::path::Path::new(path)
        .extension()
        .map_or(false, |ext| ext.eq_ignore_ascii_case("json"))
    {
        Ok((stream, HeaderValue::from_static("application/json")))
    } else if path == "/HEAD" || path.starts_with("/info") {
        Ok((stream, HeaderValue::from_static("text/plain; charset=utf-8")))
    } else {
        Ok((stream, HeaderValue::from_static("application/octet-stream")))
    }
}

fn index_serve_map_err(e: ApiError, domain: &str) -> (StatusCode, [(HeaderName, HeaderValue); 2], Json<ApiError>) {
    let (status, body) = response_error(e);
    (
        status,
        [
            (
                header::WWW_AUTHENTICATE,
                HeaderValue::from_str(&format!("Basic realm={domain}")).unwrap(),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
}

pub async fn index_serve_check_auth(
    application: &Application,
    auth_data: &AuthData,
) -> Result<(), (StatusCode, [(HeaderName, HeaderValue); 2], Json<ApiError>)> {
    application
        .authenticate(auth_data)
        .await
        .map_err(|e| index_serve_map_err(e, &application.configuration.web_domain))?;
    Ok(())
}

pub async fn index_serve(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    request: Request<Body>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 2], Body), (StatusCode, [(HeaderName, HeaderValue); 2], Json<ApiError>)> {
    let map_err = |e| index_serve_map_err(e, &state.application.configuration.web_domain);
    let path = request.uri().path();
    if path != "/config.json" && !state.application.configuration.index.allow_protocol_sparse {
        // config.json is always allowed because it is always checked first by cargo
        return Err(map_err(error_not_found()));
    }
    index_serve_check_auth(&state.application, &auth_data).await?;
    let index = state.application.index.lock().await;
    let (stream, content_type) = index_serve_inner(&index, path).await.map_err(map_err)?;
    let body = Body::from_stream(stream);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    ))
}

pub async fn index_serve_info_refs(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 2], Body), (StatusCode, [(HeaderName, HeaderValue); 2], Json<ApiError>)> {
    let map_err = |e| index_serve_map_err(e, &state.application.configuration.web_domain);
    if !state.application.configuration.index.allow_protocol_git {
        return Err(map_err(error_not_found()));
    }
    index_serve_check_auth(&state.application, &auth_data).await?;
    let index = state.application.index.lock().await;

    if query.get("service").map(String::as_str) == Some("git-upload-pack") {
        // smart server response
        let data = index.get_upload_pack_info_refs().await.map_err(map_err)?;
        Ok((
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-git-upload-pack-advertisement"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            Body::from(data),
        ))
    } else {
        // dumb server response is disabled
        Err(map_err(error_not_found()))
    }
}

pub async fn index_serve_git_upload_pack(
    auth_data: AuthData,
    State(state): State<Arc<AxumState>>,
    body: Bytes,
) -> Result<(StatusCode, [(HeaderName, HeaderValue); 2], Body), (StatusCode, [(HeaderName, HeaderValue); 2], Json<ApiError>)> {
    let map_err = |e| index_serve_map_err(e, &state.application.configuration.web_domain);
    if !state.application.configuration.index.allow_protocol_git {
        return Err(map_err(error_not_found()));
    }
    index_serve_check_auth(&state.application, &auth_data).await?;
    let index = state.application.index.lock().await;
    let data = index.get_upload_pack_for(&body).await.map_err(map_err)?;
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-git-upload-pack-result"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        Body::from(data),
    ))
}

/// Gets the version data for the application
///
/// # Errors
///
/// Always return the `Ok` variant, but use `Result` for possible future usage.
pub async fn get_version() -> ApiResult<AppVersion> {
    response(Ok(AppVersion {
        commit: crate::GIT_HASH.to_string(),
        tag: crate::GIT_TAG.to_string(),
    }))
}
