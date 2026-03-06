use crate::auth::access_keys::AccessKeyManager;
use crate::errors::OxenHttpError;
use crate::helpers::get_repo;
use crate::params::{app_data, parse_resource, path_param};

use liboxen::core::staged::with_staged_db_manager;
use liboxen::error::OxenError;
use liboxen::model::commit::NewCommitBody;
use liboxen::model::file::{FileContents, FileNew, TempFileNew};
use liboxen::model::merkle_tree::node::EMerkleTreeNode;
use liboxen::model::metadata::metadata_image::ImgResize;
use liboxen::model::metadata::metadata_video::VideoThumbnail;
use liboxen::model::{Commit, User};
use liboxen::repositories::{self, branches};
use liboxen::util;
use liboxen::view::{CommitResponse, StatusMessage};

use actix_multipart::Multipart;
use actix_web::{HttpRequest, HttpResponse, http::header, web};
use futures_util::{StreamExt, TryStreamExt as _};
use liboxen::repositories::commits;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;
use utoipa::ToSchema;

const ALLOWED_IMPORT_DOMAINS: [&str; 3] = ["huggingface.co", "kaggle.com", "oxen.ai"];

#[derive(ToSchema, Deserialize)]
#[schema(
    title = "FileUploadBody",
    description = "Body for uploading a file via multipart/form-data",
    example = json!({
        "file": "<binary data>",
        "message": "Adding a picture of a cow",
        "name": "bessie",
        "email": "bessie@oxen.ai"
    })
)]
pub struct FileUploadBody {
    #[schema(value_type = String, format = Binary)]
    pub file: Vec<u8>,
    #[schema(example = "Adding a new image to the training set")]
    pub message: Option<String>,
    #[schema(example = "bessie")]
    pub name: Option<String>,
    #[schema(example = "bessie@oxen.ai")]
    pub email: Option<String>,
}

/// Combined query parameters for file operations (image resize and video thumbnail)
/// Since both ImgResize and VideoThumbnail share width/height fields, we combine them here
#[derive(Deserialize, Debug)]
pub struct FileQueryParams {
    // Shared parameters (can be used for both image resize and video thumbnail)
    pub width: Option<u32>,
    pub height: Option<u32>,
    // Video thumbnail specific parameters
    pub timestamp: Option<f64>,
    pub thumbnail: Option<bool>,
}

/// Download File
#[utoipa::path(
    get,
    path = "/api/repos/{namespace}/{repo_name}/file/{resource}",
    tag = "Files",
    description = "Download a file from the repository. Supports image resizing and video thumbnail generation via query parameters.",
    params(
        ("namespace" = String, Path, description = "Namespace of the repository", example = "ox"),
        ("repo_name" = String, Path, description = "Name of the repository", example = "Voice-Data"),
        ("resource" = String, Path, description = "Path to the file (including branch/commit info)", example = "main/audio/moo.wav"),
        ("width" = Option<u32>, Query, description = "Width for image resize or video thumbnail", example = 320),
        ("height" = Option<u32>, Query, description = "Height for image resize or video thumbnail", example = 240),
        ("timestamp" = Option<f64>, Query, description = "Timestamp in seconds to extract video thumbnail from (default: 1.0)", example = 1.0),
        ("thumbnail" = Option<bool>, Query, description = "Set to true to generate a video thumbnail instead of returning the full video", example = true)
    ),
    responses(
        (status = 200, description = "File content stream", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 404, description = "File not found")
    )
)]
pub async fn get(
    req: HttpRequest,
    query: web::Query<FileQueryParams>,
) -> actix_web::Result<HttpResponse, OxenHttpError> {
    let app_data = app_data(&req)?;
    let namespace = path_param(&req, "namespace")?;
    let repo_name = path_param(&req, "repo_name")?;
    let repo = get_repo(&app_data.path, &namespace, &repo_name)?;
    let version_store = repo.version_store()?;
    let resource = parse_resource(&req, &repo)?;
    let workspace = resource.workspace.as_ref();
    let path = resource.path.clone();

    // Use workspace_repo for staged DB operations, base_repo for commit tree lookups
    let (staged_repo, base_repo) = match workspace {
        Some(ws) => (&ws.workspace_repo, &repo),
        None => (&repo, &repo),
    };

    let entry = match workspace {
        Some(ws) => with_staged_db_manager(staged_repo, |staged_db_manager| {
            // Try staged DB first
            if let Some(staged_node) = staged_db_manager.read_from_staged_db(&path)? {
                let file_node = match staged_node.node.node {
                    EMerkleTreeNode::File(f) => Ok(f),
                    _ => Err(OxenError::basic_str(
                        "Only single file download is supported",
                    )),
                }?;
                return Ok(file_node);
            }

            // Fall back to commit tree using workspace's commit
            let commit = &ws.commit;
            let file_node = repositories::tree::get_file_by_path(base_repo, commit, &path)?
                .ok_or(OxenError::path_does_not_exist(path.clone()))?;
            Ok(file_node)
        }),
        None => {
            let commit = resource.clone().commit.ok_or(OxenHttpError::NotFound)?;
            let file_node = repositories::tree::get_file_by_path(base_repo, &commit, &path)?
                .ok_or(OxenError::path_does_not_exist(path.clone()))?;
            Ok(file_node)
        }
    }?;

    let file_hash = entry.hash();
    let hash_str = file_hash.to_string();
    let mime_type = entry.mime_type();
    let last_commit_id = entry.last_commit_id().to_string();
    let version_path = version_store.get_version_path(&hash_str)?;

    let query_params = query.into_inner();

    // Handle image resize
    if (query_params.width.is_some() || query_params.height.is_some())
        && mime_type.starts_with("image/")
    {
        let img_resize = ImgResize {
            width: query_params.width,
            height: query_params.height,
        };
        log::debug!("img_resize {img_resize:?}");

        let file_stream = util::fs::handle_image_resize(
            Arc::clone(&version_store),
            hash_str.clone(),
            &path,
            &version_path,
            img_resize,
        )
        .await?;

        return Ok(HttpResponse::Ok()
            .content_type(mime_type)
            .insert_header(("oxen-revision-id", last_commit_id.as_str()))
            .streaming(file_stream));
    }

    // Handle video thumbnail - requires thumbnail=true parameter
    if query_params.thumbnail == Some(true) && mime_type.starts_with("video/") {
        let video_thumbnail = VideoThumbnail {
            width: query_params.width,
            height: query_params.height,
            timestamp: query_params.timestamp,
            thumbnail: query_params.thumbnail,
        };
        log::debug!("video_thumbnail {video_thumbnail:?}");

        let thumbnail_path = util::fs::handle_video_thumbnail(
            Arc::clone(&version_store),
            hash_str,
            &path,
            &version_path,
            video_thumbnail,
        )?;
        log::debug!("In the thumbnail cache! {thumbnail_path:?}");

        // Generate stream for the thumbnail (always JPEG)
        let file = File::open(&thumbnail_path).await?;
        let reader = BufReader::new(file);
        let stream = ReaderStream::new(reader);

        return Ok(HttpResponse::Ok()
            .content_type("image/jpeg")
            .insert_header(("oxen-revision-id", last_commit_id.as_str()))
            .streaming(stream));
    }

    log::debug!("did not hit the resize or thumbnail cache");

    // Stream the file
    let stream = version_store.get_version_stream(&hash_str).await?;

    Ok(HttpResponse::Ok()
        .content_type(mime_type)
        .insert_header(("oxen-revision-id", last_commit_id.as_str()))
        .streaming(stream))
}

/// Upload files
#[utoipa::path(
    put,
    path = "/api/repos/{namespace}/{repo_name}/file/{resource}",
    tag = "Files",
    description = "Upload files to a directory on a branch via multipart form and commit them.",
    params(
        ("namespace" = String, Path, description = "Namespace of the repository", example = "ox"),
        ("repo_name" = String, Path, description = "Name of the repository", example = "ImageNet-1k"),
        ("resource" = String, Path, description = "Path of the directory to add files in (including branch)", example = "main/train/images"),
    ),
    request_body(
        content_type = "multipart/form-data",
        content = FileUploadBody
    ),
    responses(
        (status = 200, description = "Files committed successfully", body = CommitResponse),
        (status = 400, description = "Bad Request"),
        (status = 404, description = "Branch or path not found")
    )
)]
pub async fn put(
    req: HttpRequest,
    payload: web::Payload,
) -> actix_web::Result<HttpResponse, OxenHttpError> {
    log::debug!("file::put path {:?}", req.path());

    let app_data = app_data(&req)?;
    let namespace = path_param(&req, "namespace")?;
    let repo_name = path_param(&req, "repo_name")?;
    let repo = get_repo(&app_data.path, &namespace, &repo_name)?;

    // If there's no head commit, handle initial upload
    if repositories::commits::head_commit_maybe(&repo)?.is_none() {
        return handle_initial_put_empty_repo(req, payload, &repo).await;
    }

    let resource = match parse_resource(&req, &repo) {
        Ok(res) => res,
        Err(parse_err) => {
            return Err(parse_err);
        }
    };

    // Resource must specify branch because we need to commit the workspace back to a branch
    let branch = resource
        .branch
        .clone()
        .ok_or(OxenError::local_branch_not_found(
            resource.version.to_string_lossy(),
        ))?;
    let commit = resource.commit.ok_or(OxenHttpError::NotFound)?;

    // Extract claimed commit hash from HTTP header
    let claimed_commit_hash = req
        .headers()
        .get("oxen-based-on")
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string());

    // Check if the resource path is a file and handle conflicts
    let node = repositories::tree::get_node_by_path(&repo, &commit, &resource.path)?;
    if let Some(node) = node
        && node.is_file()
    {
        // Get current commit hash for the file
        let current_commit_hash = node.latest_commit_id()?.to_string();

        // Only fail if claimed hash is provided but doesn't match current hash
        if let Some(claimed_hash) = claimed_commit_hash
            && current_commit_hash != claimed_hash
        {
            return Err(OxenHttpError::BasicError(
                format!(
                    "File has been modified since claimed revision. Current: {}, Claimed: {}. Your changes would overwrite another change without that being from a merge",
                    current_commit_hash, claimed_hash
                )
                .into(),
            ));
        }
    }

    // Try to get commit message from header first (for backwards compatibility)
    let header_message = req
        .headers()
        .get("oxen-commit-message")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Optional commit info from headers
    //TODO: cease using header_author and  header_email below, instead take from authenticated_user var below

    // Parse payload based on content type
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|ct| ct.to_str().ok())
        .unwrap_or("");

    let (message, temp_files) = if content_type.starts_with("multipart/form-data") {
        // Handle multipart data
        let multipart = Multipart::new(req.headers(), payload);
        parse_multipart_fields(multipart).await?
    } else {
        // Handle raw payload
        parse_raw_payload(&req, payload).await?
    };

    // Get authenticated user from bearer token
    let authenticated_user = get_authenticated_user(&req)?;

    // If header message is provided, it must be valid and non-empty (backwards compatibility)
    if req.headers().contains_key("oxen-commit-message") {
        if header_message.is_none() {
            log::warn!("💬 Invalid oxen-commit-message header provided");
            return Err(OxenHttpError::BadRequest(
                "Invalid oxen-commit-message header value".into(),
            ));
        }
        if let Some(ref msg) = header_message
            && msg.trim().is_empty()
        {
            log::warn!("💬 Empty oxen-commit-message header provided");
            return Err(OxenHttpError::BadRequest(
                "Invalid oxen-commit-message header value".into(),
            ));
        }
    }

    // Use authenticated user if available, otherwise require authentication
    let user = match authenticated_user {
        Some(user) => user,
        None => {
            return Err(OxenHttpError::BadRequest(
                "Bearer token required for PUT operations".into(),
            ));
        }
    };

    let mut files: Vec<FileNew> = vec![];
    for temp_file in temp_files {
        files.push(FileNew {
            path: temp_file.path,
            contents: temp_file.contents,
            user: user.clone(), // Clone the user for each file
        });
    }
    let workspace = repositories::workspaces::create_temporary(&repo, &commit)?;

    process_and_add_files(
        &repo,
        Some(&workspace),
        resource.path.clone(),
        files.clone(),
    )
    .await?;

    // Commit workspace
    let commit_body = NewCommitBody {
        author: user.name.clone(),
        email: user.email.clone(),
        message: header_message.or(message).unwrap_or(format!(
            "Auto-commit files to {}",
            &resource.path.to_string_lossy()
        )),
    };

    let commit = repositories::workspaces::commit(&workspace, &commit_body, branch.name).await?;

    log::debug!("file::put workspace commit ✅ success! commit {commit:?}");

    Ok(HttpResponse::Ok().json(CommitResponse {
        status: StatusMessage::resource_created(),
        commit,
    }))
}

/// Delete file
#[utoipa::path(
    delete,
    path = "/api/repos/{namespace}/{repo_name}/file/{resource}",
    description = "Remove a file from the repository. Stage the file as removed to a workspace and commit the removal.",
    tag = "Files",
    params(
        ("namespace" = String, Path, description = "Namespace of the repository", example = "ox"),
        ("repo_name" = String, Path, description = "Name of the repository", example = "ImageNet-1k"),
        ("resource" = String, Path, description = "Path to the file to be deleted (including branch)", example = "main/train/images/n01440764_10026.JPEG"),
    ),
    responses(
        (status = 200, description = "File removed successfully", body = CommitResponse),
        (status = 404, description = "Branch or path not found")
    )
)]
pub async fn delete(req: HttpRequest) -> actix_web::Result<HttpResponse, OxenHttpError> {
    log::debug!("file::delete path {:?}", req.path());
    let app_data = app_data(&req)?;
    let namespace = path_param(&req, "namespace")?;
    let repo_name = path_param(&req, "repo_name")?;
    let repo = get_repo(&app_data.path, &namespace, &repo_name)?;

    // Parse the resource (branch/commit/path) - DELETE operations require existing commits
    let resource = parse_resource(&req, &repo)?;

    // Resource must specify branch because we need to commit the workspace back to a branch
    let branch = resource
        .branch
        .clone()
        .ok_or(OxenError::local_branch_not_found(
            resource.version.to_string_lossy(),
        ))?;
    let commit = resource.commit.ok_or(OxenHttpError::NotFound)?;

    // Extract claimed commit hash from HTTP header
    let claimed_commit_hash = req
        .headers()
        .get("oxen-based-on")
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string());

    // Check if the resource path exists and is a file
    let node = repositories::tree::get_node_by_path(&repo, &commit, &resource.path)?;
    let node = node.ok_or_else(|| OxenHttpError::NotFound)?;

    if !node.is_file() {
        return Err(OxenHttpError::BadRequest(
            format!("Cannot delete directory: {}", resource.path.display()).into(),
        ));
    }

    // Get current commit hash for the file and validate oxen-based-on header if provided
    let current_commit_hash = node.latest_commit_id()?.to_string();
    if let Some(claimed_hash) = claimed_commit_hash
        && current_commit_hash != claimed_hash
    {
        return Err(OxenHttpError::BasicError(
            format!(
                "File has been modified since claimed revision. Current: {}, Claimed: {}. Your changes would overwrite another change without that being from a merge",
                current_commit_hash, claimed_hash
            )
            .into(),
        ));
    }

    // Get authenticated user from bearer token
    let authenticated_user = get_authenticated_user(&req)?;
    let user = match authenticated_user {
        Some(user) => user,
        None => {
            return Err(OxenHttpError::BadRequest(
                "Bearer token required for DELETE operations".into(),
            ));
        }
    };

    // Create temporary workspace
    let workspace = repositories::workspaces::create_temporary(&repo, &commit)?;

    // Stage the deletion using the relative path (not absolute workspace path)
    repositories::workspaces::files::rm(&workspace, &resource.path).await?;

    // Commit workspace with deletion
    let commit_body = NewCommitBody {
        author: user.name.clone(),
        email: user.email.clone(),
        message: format!("Delete file {}", resource.path.display()),
    };

    let commit = repositories::workspaces::commit(&workspace, &commit_body, branch.name).await?;

    log::debug!(
        "file::delete workspace commit ✅ success! commit {:?}",
        commit
    );

    Ok(HttpResponse::Ok().json(CommitResponse {
        status: StatusMessage::resource_deleted(),
        commit,
    }))
}

#[derive(ToSchema, Deserialize)]
#[schema(
    title = "FileMoveBody",
    description = "Body for moving/renaming a file",
    example = json!({
        "new_path": "new/path/to/file.txt",
        "message": "Renamed file to new location",
        "name": "bessie",
        "email": "bessie@oxen.ai"
    })
)]
pub struct FileMoveBody {
    #[schema(example = "new/path/to/file.txt")]
    pub new_path: String,
    #[schema(example = "Moved file to new location")]
    pub message: Option<String>,
    #[schema(example = "bessie")]
    pub name: Option<String>,
    #[schema(example = "bessie@oxen.ai")]
    pub email: Option<String>,
}

/// Move/Rename file
#[utoipa::path(
    patch,
    path = "/api/repos/{namespace}/{repo_name}/file/{resource}",
    tag = "Files",
    description = "Move or rename a file within the repository and commit the change.",
    params(
        ("namespace" = String, Path, description = "Namespace of the repository", example = "ox"),
        ("repo_name" = String, Path, description = "Name of the repository", example = "ImageNet-1k"),
        ("resource" = String, Path, description = "Path to the source file (including branch)", example = "main/train/images/old_name.jpg"),
    ),
    request_body(
        content_type = "application/json",
        content = FileMoveBody
    ),
    responses(
        (status = 200, description = "File moved/renamed successfully", body = CommitResponse),
        (status = 400, description = "Bad Request"),
        (status = 404, description = "Branch or file not found")
    )
)]
pub async fn mv(req: HttpRequest, body: String) -> actix_web::Result<HttpResponse, OxenHttpError> {
    log::debug!("file::mv path {:?}", req.path());
    let app_data = app_data(&req)?;
    let namespace = path_param(&req, "namespace")?;
    let repo_name = path_param(&req, "repo_name")?;
    let repo = get_repo(&app_data.path, &namespace, &repo_name)?;

    // Parse the resource (branch/commit/path)
    let resource = parse_resource(&req, &repo)?;

    // Resource must specify branch because we need to commit the workspace back to a branch
    let branch = resource
        .branch
        .clone()
        .ok_or(OxenError::local_branch_not_found(
            resource.version.to_string_lossy(),
        ))?;
    let commit = resource.commit.clone().ok_or(OxenHttpError::NotFound)?;
    let source_path = resource.path;

    // Parse the request body
    let body: FileMoveBody = serde_json::from_str(&body)?;

    // Validate new_path is not empty
    if body.new_path.is_empty() {
        return Err(OxenHttpError::BadRequest("new_path cannot be empty".into()));
    }

    // Validate and normalize new_path
    let new_path = util::fs::validate_and_normalize_path(&body.new_path)?;

    // Verify source file exists
    if repositories::entries::get_file(&repo, &commit, &source_path)?.is_none() {
        return Err(OxenHttpError::NotFound);
    }

    // Check if new_path already exists (file OR directory)
    if repositories::tree::get_node_by_path(&repo, &commit, &new_path)?.is_some() {
        return Err(OxenHttpError::BadRequest(
            "new_path already exists in the repository".into(),
        ));
    }

    log::debug!("file::mv creating workspace for commit: {commit}");
    let workspace = repositories::workspaces::create_temporary(&repo, &commit)?;

    // Stage the move
    log::debug!("file::mv moving {source_path:?} to {new_path:?}");
    repositories::workspaces::files::mv(&workspace, &source_path, &new_path)?;

    // Commit workspace
    let commit_body = NewCommitBody {
        author: body.name.clone().unwrap_or_default(),
        email: body.email.clone().unwrap_or_default(),
        message: body.message.clone().unwrap_or_else(|| {
            format!(
                "Move {} to {}",
                source_path.to_string_lossy(),
                new_path.to_string_lossy()
            )
        }),
    };

    let commit = repositories::workspaces::commit(&workspace, &commit_body, branch.name).await?;

    log::debug!("file::mv workspace commit ✅ success! commit {commit:?}");

    Ok(HttpResponse::Ok().json(CommitResponse {
        status: StatusMessage::resource_updated(),
        commit,
    }))
}

// Helper: when the repository has no commits yet, accept the upload as the first commit
async fn handle_initial_put_empty_repo(
    req: HttpRequest,
    payload: web::Payload,
    repo: &liboxen::model::LocalRepository,
) -> actix_web::Result<HttpResponse, OxenHttpError> {
    let resource: PathBuf = PathBuf::from(req.match_info().query("resource"));

    let mut resource_components = resource.components();
    let branch_name = resource_components
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or("main".to_string());
    let path_string = resource_components
        .collect::<PathBuf>()
        .to_string_lossy()
        .to_string();
    let path = PathBuf::from(path_string);

    // Parse payload based on content type
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|ct| ct.to_str().ok())
        .unwrap_or("");

    let (_message, temp_files) = if content_type.starts_with("multipart/form-data") {
        // Handle multipart data
        let multipart = Multipart::new(req.headers(), payload);
        parse_multipart_fields(multipart).await?
    } else {
        // Handle raw payload
        parse_raw_payload(&req, payload).await?
    };

    // Get authenticated user from bearer token
    let authenticated_user = get_authenticated_user(&req)?;
    let user = match authenticated_user {
        Some(user) => user,
        None => {
            return Err(OxenHttpError::BadRequest(
                "Bearer token required for PUT operations".into(),
            ));
        }
    };

    // Convert temporary files to FileNew with the complete user information
    let mut files: Vec<FileNew> = vec![];
    for temp_file in temp_files {
        files.push(FileNew {
            path: temp_file.path,
            contents: temp_file.contents,
            user: user.clone(), // Clone the user for each file
        });
    }

    // If the user supplied files, add and commit them
    let mut commit: Option<Commit> = None;

    process_and_add_files(repo, None, path, files.clone()).await?;

    if !files.is_empty() {
        let user_ref = &files[0].user; // Use the user from the first file, since it's the same for all
        commit = Some(commits::commit_with_user(repo, "Initial commit", user_ref)?);
        branches::create(repo, &branch_name, &commit.as_ref().unwrap().id)?;
    }

    Ok(HttpResponse::Ok().json(CommitResponse {
        status: StatusMessage::resource_created(),
        commit: commit.unwrap(),
    }))
}

/// import files from hf/kaggle (create a workspace and commit)
pub async fn import(
    req: HttpRequest,
    body: web::Json<Value>,
) -> Result<HttpResponse, OxenHttpError> {
    let app_data = app_data(&req)?;
    let namespace = path_param(&req, "namespace")?;
    let repo_name = path_param(&req, "repo_name")?;
    let repo = get_repo(&app_data.path, namespace, &repo_name)?;
    let resource = parse_resource(&req, &repo)?;

    // Resource must specify branch for committing the workspace
    let branch = resource
        .branch
        .clone()
        .ok_or(OxenError::local_branch_not_found(
            resource.version.to_string_lossy(),
        ))?;
    let commit = resource.commit.ok_or(OxenHttpError::NotFound)?;
    let directory = resource.path.clone();
    log::debug!("workspace::files::import_file Got directory: {directory:?}");

    // commit info
    let author = req.headers().get("oxen-commit-author");
    let email = req.headers().get("oxen-commit-email");
    let message = req.headers().get("oxen-commit-message");

    log::debug!(
        "file::import commit info author:{:?}, email:{:?}, message:{:?}",
        author,
        email,
        message
    );

    // Make sure the resource path is not already a file
    let node = repositories::tree::get_node_by_path(&repo, &commit, &resource.path)?;
    if node.is_some() && node.unwrap().is_file() {
        return Err(OxenHttpError::BasicError(
            format!(
                "Target path must be a directory: {}",
                resource.path.display()
            )
            .into(),
        ));
    }

    // Create temporary workspace
    let workspace = repositories::workspaces::create_temporary(&repo, &commit)?;

    log::debug!("workspace::files::import_file workspace created!");

    // extract auth key from req body
    let auth = body
        .get("headers")
        .and_then(|headers| headers.as_object())
        .and_then(|map| map.get("Authorization"))
        .and_then(|auth| auth.as_str())
        .unwrap_or_default();

    let download_url = body
        .get("download_url")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    // Validate URL domain
    let url_parsed = url::Url::parse(download_url)
        .map_err(|_| OxenHttpError::BadRequest("Invalid URL".into()))?;
    let domain = url_parsed
        .domain()
        .ok_or_else(|| OxenHttpError::BadRequest("Invalid URL domain".into()))?;
    if !ALLOWED_IMPORT_DOMAINS.iter().any(|&d| domain.ends_with(d)) {
        return Err(OxenHttpError::BadRequest("URL domain not allowed".into()));
    }

    // parse filename from the given url
    let filename = if url_parsed.domain() == Some("huggingface.co") {
        url_parsed.path_segments().and_then(|segments| {
            let segments: Vec<_> = segments.collect();
            if segments.len() >= 2 {
                let last_two = &segments[segments.len() - 2..];
                Some(format!("{}_{}", last_two[0], last_two[1]))
            } else {
                None
            }
        })
    } else {
        url_parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .map(|s| s.to_string())
    }
    .ok_or_else(|| OxenHttpError::BadRequest("Invalid filename in URL".into()))?;

    // download and save the file into the workspace
    repositories::workspaces::files::import(download_url, auth, directory, filename, &workspace)
        .await?;

    // Commit workspace
    let commit_body = NewCommitBody {
        author: author.map_or("".to_string(), |a| a.to_str().unwrap().to_string()),
        email: email.map_or("".to_string(), |e| e.to_str().unwrap().to_string()),
        message: message.map_or(
            format!("Import files to {}", &resource.path.to_string_lossy()),
            |m| m.to_str().unwrap().to_string(),
        ),
    };

    let commit = repositories::workspaces::commit(&workspace, &commit_body, branch.name).await?;
    log::debug!("workspace::commit ✅ success! commit {:?}", commit);

    Ok(HttpResponse::Ok().json(CommitResponse {
        status: StatusMessage::resource_created(),
        commit,
    }))
}

// Helper function to extract authenticated user from bearer token
fn get_authenticated_user(req: &HttpRequest) -> Result<Option<User>, OxenHttpError> {
    // Extract bearer token from Authorization header
    let auth_header = req.headers().get("authorization");

    if let Some(auth_value) = auth_header {
        if let Ok(auth_str) = auth_value.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                let app_data = app_data(req)?;

                log::debug!(
                    "Attempting to validate bearer token: {}...",
                    &token[..std::cmp::min(20, token.len())]
                );
                log::debug!("AccessKeyManager path: {:?}", &app_data.path);

                match AccessKeyManager::new_read_only(&app_data.path) {
                    Ok(keygen) => {
                        log::debug!("AccessKeyManager created successfully");
                        match keygen.get_claim(token) {
                            Ok(Some(claim)) => {
                                log::debug!(
                                    "Token validated successfully for user: {}",
                                    claim.name()
                                );
                                return Ok(Some(User {
                                    name: claim.name().to_string(),
                                    email: claim.email().to_string(),
                                }));
                            }
                            Ok(None) => {
                                log::debug!("Token validation returned None");
                            }
                            Err(e) => {
                                log::debug!("Token validation error: {:?}", e);
                            }
                        }
                    }
                    Err(err) => {
                        log::debug!("AccessKeyManager creation failed: {:?}", err);
                        // Treat missing keys DB as "no authentication configured" instead of crashing
                    }
                }
            } else {
                log::debug!("Authorization header does not start with 'Bearer '");
            }
        } else {
            log::debug!("Could not parse authorization header as string");
        }
    } else {
        log::debug!("No authorization header found");
    }

    Ok(None)
}

async fn parse_multipart_fields(
    mut payload: Multipart,
) -> actix_web::Result<(Option<String>, Vec<TempFileNew>), OxenHttpError> {
    let mut message: Option<String> = None;
    let mut temp_files: Vec<TempFileNew> = vec![];

    while let Some(mut field) = payload
        .try_next()
        .await
        .map_err(OxenHttpError::MultipartError)?
    {
        let disposition = field.content_disposition().ok_or(OxenHttpError::NotFound)?;
        let field_name = disposition
            .get_name()
            .ok_or(OxenHttpError::NotFound)?
            .to_string();

        match field_name.as_str() {
            "name" | "email" => {
                // Skip name and email fields - they come from authenticated user
                while let Some(_chunk) = field
                    .try_next()
                    .await
                    .map_err(OxenHttpError::MultipartError)?
                {
                    // Just consume the field data
                }
            }
            "message" => {
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .try_next()
                    .await
                    .map_err(OxenHttpError::MultipartError)?
                {
                    bytes.extend_from_slice(&chunk);
                }
                let value = String::from_utf8(bytes)
                    .map_err(|e| OxenHttpError::BadRequest(e.to_string().into()))?;
                message = Some(value);
            }
            "files[]" | "file" => {
                let filename = disposition.get_filename().map_or_else(
                    || uuid::Uuid::new_v4().to_string(),
                    sanitize_filename::sanitize,
                );

                let mut contents = Vec::new();
                while let Some(chunk) = field
                    .try_next()
                    .await
                    .map_err(OxenHttpError::MultipartError)?
                {
                    contents.extend_from_slice(&chunk);
                }

                temp_files.push(TempFileNew {
                    path: PathBuf::from(&filename),
                    contents: FileContents::Binary(contents),
                });
            }
            _ => {}
        }
    }

    Ok((message, temp_files))
}

async fn parse_raw_payload(
    req: &HttpRequest,
    mut payload: web::Payload,
) -> actix_web::Result<(Option<String>, Vec<TempFileNew>), OxenHttpError> {
    // Extract file path from the URL
    let path_info = req.path();
    // Extract the filename from the last part of the path
    let filename = path_info
        .split('/')
        .next_back()
        .unwrap_or("file")
        .to_string();

    // Check if the path ends with '/' which indicates a directory
    if path_info.ends_with('/') {
        return Err(OxenHttpError::BadRequest(
            "Cannot PUT to a directory path. Path cannot end with '/'".into(),
        ));
    }

    // Collect the raw payload bytes
    let mut bytes = web::BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk =
            chunk.map_err(|e| OxenHttpError::BadRequest(format!("Payload error: {}", e).into()))?;
        bytes.extend_from_slice(&chunk);
    }

    // Create a temporary file from the raw bytes
    let temp_file = TempFileNew {
        path: std::path::PathBuf::from(&filename),
        contents: FileContents::Text(String::from_utf8_lossy(&bytes).to_string()),
    };

    // Extract commit message from header (optional)
    let message = req
        .headers()
        .get("oxen-commit-message")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    Ok((message, vec![temp_file]))
}

// Helper function for processing files and adding to repo/workspace
async fn process_and_add_files(
    repo: &liboxen::model::LocalRepository,
    workspace: Option<&liboxen::repositories::workspaces::TemporaryWorkspace>,
    base_path: PathBuf,
    files: Vec<FileNew>,
) -> Result<(), OxenError> {
    if !files.is_empty() {
        log::debug!(
            "process_and_add_files() processing {} files to base_path: {:?}",
            files.len(),
            base_path
        );
        for file in files.clone() {
            let contents = &file.contents;

            // The base_path from the URL is the definitive path for the file.
            // The filename from multipart is ignored to avoid ambiguity.
            let full_path_in_dest = if let Some(ws) = workspace {
                ws.dir().join(&base_path)
            } else {
                repo.path.join(&base_path)
            };

            log::debug!(
                "process_and_add_files() full_path_in_dest: {:?}",
                full_path_in_dest
            );

            // Create parent directory if it doesn't exist
            if let Some(parent) = full_path_in_dest.parent()
                && !parent.exists()
            {
                log::debug!("process_and_add_files() creating parent dir: {:?}", parent);
                util::fs::create_dir_all(parent)?;
            }

            // Write the file contents
            match contents {
                FileContents::Text(text) => {
                    util::fs::write(&full_path_in_dest, text.as_bytes())?;
                }
                FileContents::Binary(bytes) => {
                    util::fs::write(&full_path_in_dest, bytes)?;
                }
            }

            // Add the file to staging
            if let Some(ws) = workspace {
                repositories::workspaces::files::add(ws, &full_path_in_dest).await?;
            } else {
                repositories::add(repo, &full_path_in_dest).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::test;
    use std::path::PathBuf;

    use actix_multipart_test::MultiPartFormDataBuilder;
    use actix_web::{App, web};
    use liboxen::view::CommitResponse;

    use liboxen::error::OxenError;
    use liboxen::repositories;
    use liboxen::util;

    use crate::app_data::OxenAppData;
    use crate::controllers;

    #[actix_web::test]
    async fn test_controllers_file_put() -> Result<(), OxenError> {
        liboxen::test::init_test_env();
        let sync_dir = test::get_sync_dir()?;
        let namespace = "Testing-Namespace";
        let repo_name = "Testing-Name";
        let repo = test::create_local_repo(&sync_dir, namespace, repo_name)?;
        util::fs::create_dir_all(repo.path.join("data"))?;
        let hello_file = repo.path.join("data/hello.txt");
        util::fs::write_to_path(&hello_file, "Hello")?;
        repositories::add(&repo, &hello_file).await?;
        let _commit = repositories::commit(&repo, "First commit")?;

        util::fs::write_to_path(&hello_file, "Updated Content!")?;
        let mut multipart_form_data_builder = MultiPartFormDataBuilder::new();
        multipart_form_data_builder.with_file(
            hello_file,   // First argument: Path to the actual file on disk
            "file",       // Second argument: Field name (as expected by your server)
            "text/plain", // Content type
            "hello.txt",  // Filename for the multipart form
        );
        multipart_form_data_builder.with_text("name", "some_name");
        multipart_form_data_builder.with_text("email", "some_email");
        multipart_form_data_builder.with_text("message", "some_message");
        let (header, body) = multipart_form_data_builder.build();
        let uri = format!("/oxen/{namespace}/{repo_name}/file/main/data");
        let req = actix_web::test::TestRequest::put()
            .uri(&uri)
            .app_data(OxenAppData::new(sync_dir.to_path_buf()))
            .param("namespace", namespace)
            .param("resource", "data")
            .param("repo_name", repo_name);

        let req = req.insert_header(header).set_payload(body).to_request();

        let app = actix_web::test::init_service(
            App::new()
                .app_data(OxenAppData::new(sync_dir.clone()))
                .route(
                    "/oxen/{namespace}/{repo_name}/file/{resource:.*}",
                    web::put().to(controllers::file::put),
                ),
        )
        .await;

        let resp = actix_web::test::call_service(&app, req).await;
        let bytes = actix_http::body::to_bytes(resp.into_body()).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        let resp: CommitResponse = serde_json::from_str(body)?;
        assert_eq!(resp.status.status, "success");

        // Check that the file was updated
        let entry =
            repositories::entries::get_file(&repo, &resp.commit, PathBuf::from("data/hello.txt"))?
                .unwrap();
        let version_store = repo.version_store()?;
        let uploaded_content = version_store.get_version(&entry.hash().to_string()).await?;
        assert_eq!(
            String::from_utf8(uploaded_content).unwrap(),
            "Updated Content!"
        );

        // cleanup
        test::cleanup_sync_dir(&sync_dir)?;

        Ok(())
    }

    #[actix_web::test]
    async fn test_controllers_file_import() -> Result<(), OxenError> {
        liboxen::test::init_test_env();
        let sync_dir = test::get_sync_dir()?;
        let namespace = "Testing-Namespace";
        let repo_name = "Testing-Name";
        let author = "test_user";
        let email = "ox@oxen.ai";
        let repo = test::create_local_repo(&sync_dir, namespace, repo_name)?;
        util::fs::create_dir_all(repo.path.join("data"))?;
        let hello_file = repo.path.join("data/hello.txt");
        util::fs::write_to_path(&hello_file, "Hello")?;
        repositories::add(&repo, &hello_file).await?;
        let _commit = repositories::commit(&repo, "First commit")?;

        let uri = format!("/oxen/{namespace}/{repo_name}/file/import/main/data");

        // import a file from oxen for testing
        let body = serde_json::json!({"download_url": "https://hub.oxen.ai/api/repos/datasets/GettingStarted/file/main/tables/cats_vs_dogs.tsv"});

        let req = actix_web::test::TestRequest::post()
            .uri(&uri)
            .app_data(OxenAppData::new(sync_dir.to_path_buf()))
            .param("namespace", namespace)
            .param("repo_name", repo_name)
            .insert_header(("oxen-commit-author", author))
            .insert_header(("oxen-commit-email", email))
            .set_json(&body)
            .to_request();

        let app = actix_web::test::init_service(
            App::new()
                .app_data(OxenAppData::new(sync_dir.clone()))
                .route(
                    "/oxen/{namespace}/{repo_name}/file/import/{resource:.*}",
                    web::post().to(controllers::file::import),
                ),
        )
        .await;

        let resp = actix_web::test::call_service(&app, req).await;
        let bytes = actix_http::body::to_bytes(resp.into_body()).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        let resp: CommitResponse = serde_json::from_str(body)?;
        assert_eq!(resp.status.status, "success");

        let entry = repositories::entries::get_file(
            &repo,
            &resp.commit,
            PathBuf::from("data/cats_vs_dogs.tsv"),
        )?
        .unwrap();
        let version_path = util::fs::version_path_from_hash(&repo, entry.hash().to_string());
        assert!(version_path.exists());

        // cleanup
        test::cleanup_sync_dir(&sync_dir)?;

        Ok(())
    }
}
