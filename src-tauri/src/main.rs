#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use http_body_util::{Full, BodyExt};
use hyper_util::rt::TokioIo;
use percent_encoding::{percent_decode, percent_encode, NON_ALPHANUMERIC};
use qrcode::QrCode;
use local_ip_address::local_ip;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tauri::{Emitter, Manager};

type BoxBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, std::convert::Infallible>;

#[derive(Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub url: String,
    pub port: u16,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

struct AppState {
    shared_folder: Arc<Mutex<Option<PathBuf>>>,
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    app_handle: tauri::AppHandle,
}


#[tauri::command]
async fn start_server(
    folder_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<ServerInfo, String> {
    let path = PathBuf::from(&folder_path);
    if !path.exists() || !path.is_dir() {
        return Err("Invalid folder path".to_string());
    }

    // Stop existing server if running
    {
        let mut handle = state.server_handle.lock().unwrap();
        if let Some(h) = handle.take() {
            h.abort();
        }
    }

    *state.shared_folder.lock().unwrap() = Some(path);

    let port = find_available_port().await?;
    let shared_folder = state.shared_folder.clone();
    let app_handle = state.app_handle.clone();

    let handle = tokio::spawn(async move {
        run_server(port, shared_folder, app_handle).await;
    });

    *state.server_handle.lock().unwrap() = Some(handle);

    let ip = local_ip().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
    Ok(ServerInfo {
        url: format!("http://{}:{}", ip, port),
        port,
    })
}

#[tauri::command]
async fn stop_server(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut handle = state.server_handle.lock().unwrap();
    if let Some(h) = handle.take() {
        h.abort();
    }
    *state.shared_folder.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
async fn generate_qr_code(url: String) -> Result<String, String> {
    let qr = QrCode::new(&url).map_err(|e| e.to_string())?;
    // Render to a string of characters
    let qr_string = qr.render::<char>().quiet_zone(false).build();

    // Parse the string to create an image
    let lines: Vec<&str> = qr_string.lines().collect();
    let size = lines.len() as u32;
    let mut img = image::GrayImage::new(size, size);

    for (y, line) in lines.iter().enumerate() {
        for (x, ch) in line.chars().enumerate() {
            let pixel = if ch == '█' || ch == '#' { 255 } else { 0 };
            img.put_pixel(x as u32, y as u32, image::Luma([pixel]));
        }
    }

    // Resize to smaller size
    let resized = image::imageops::resize(&img, 200, 200, image::imageops::FilterType::Nearest);

    // Convert to base64
    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    resized.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;

    let base64_string = base64_simd::STANDARD.encode_to_string(&png_bytes);
    Ok(format!("data:image/png;base64,{}", base64_string))
}

async fn find_available_port() -> Result<u16, String> {
    for port in 8000..9000 {
        if tokio::net::TcpListener::bind(("0.0.0.0", port)).await.is_ok() {
            return Ok(port);
        }
    }
    Err("No available port".to_string())
}

async fn run_server(port: u16, shared_folder: Arc<Mutex<Option<PathBuf>>>, app_handle: tauri::AppHandle) {
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    let listener = TcpListener::bind(addr).await.unwrap();

    while let Ok((stream, _)) = listener.accept().await {
        let io = TokioIo::new(stream);
        let shared_folder = shared_folder.clone();
        let app_handle = app_handle.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                handle_request(req, shared_folder.clone(), app_handle.clone())
            });

            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// 解析 application/x-www-form-urlencoded 风格的参数值：
/// 先把 `+` 还原为空格，再做百分号解码，兼容 encodeURIComponent 与其它客户端。
fn decode_form_value(raw: &str) -> String {
    let plus_replaced = raw.replace('+', " ");
    percent_decode(plus_replaced.as_bytes())
        .decode_utf8()
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| plus_replaced)
}

/// 生成符合 RFC 6266 的 Content-Disposition。
/// filename 用纯 ASCII 兜底，filename* 携带 UTF-8 百分号编码，浏览器据此显示正确的中文名。
/// 注意：HTTP 头值不允许出现非 ASCII 字节，直接把中文塞进 header 会导致 unwrap() panic。
fn build_content_disposition(filename: &str) -> String {
    let ascii_fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback,
        percent_encode(filename.as_bytes(), NON_ALPHANUMERIC)
    )
}

/// 只允许不含路径跳转的纯相对文件名，避免 `../` 越权读取共享目录之外的文件。
fn is_safe_relative_name(name: &str) -> bool {
    let path = std::path::Path::new(name);
    !name.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    shared_folder: Arc<Mutex<Option<PathBuf>>>,
    app_handle: tauri::AppHandle,
) -> Result<Response<BoxBody>, hyper::Error> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();

    // Clone the path and release the lock before any await
    let base_path = {
        let folder = shared_folder.lock().unwrap();
        if let Some(ref p) = *folder {
            p.clone()
        } else {
            drop(folder);
            return Ok(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Full::new("No folder selected".into()).boxed())
                .unwrap());
        }
    };

    match (method.as_str(), path) {
        ("GET", "/") => {
            let html = include_str!("../mobile/index.html");
            Ok(Response::builder()
                .header("content-type", "text/html; charset=utf-8")
                .body(Full::new(html.into()).boxed())
                .unwrap())
        }
        ("GET", "/api/files") => {
            let files = list_files(&base_path);
            let json = serde_json::to_string(&files).unwrap();
            Ok(Response::builder()
                .header("content-type", "application/json")
                .header("Access-Control-Allow-Origin", "*")
                .body(Full::new(json.into()).boxed())
                .unwrap())
        }
        ("GET", p) if p.starts_with("/api/download") => {
            if let Some(query) = uri.query() {
                // splitn(2, '=') 保证文件名本身含 '=' 时不会被截断
                let params: HashMap<String, String> = query
                    .split('&')
                    .filter_map(|s| {
                        let mut parts = s.splitn(2, '=');
                        Some((parts.next()?.to_string(), parts.next()?.to_string()))
                    })
                    .collect();

                if let Some(encoded_filename) = params.get("file") {
                    // 移动端 encodeURIComponent 后是百分号编码，先解码还原真实文件名
                    let filename = decode_form_value(encoded_filename);

                    // 拦截 ../ 等路径穿越，只允许共享目录内的普通文件名
                    if !is_safe_relative_name(&filename) {
                        return Ok(Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Full::new("Invalid filename".into()).boxed())
                            .unwrap());
                    }

                    let file_path = base_path.join(&filename);
                    if let Ok(contents) = fs::read(&file_path) {
                        return Ok(Response::builder()
                            .header("content-type", "application/octet-stream")
                            // 用解码后的真实文件名生成 header，浏览器才能显示正确的中文名
                            .header("content-disposition", build_content_disposition(&filename))
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Full::new(contents.into()).boxed())
                            .unwrap());
                    }
                }
            }
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new("Not found".into()).boxed())
                .unwrap())
        }
        ("POST", "/api/upload") => {
            let boundary = get_boundary(&req);

            if boundary.is_none() {
                eprintln!("Upload failed: No boundary found in Content-Type");
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Full::new(serde_json::json!({"success": false, "error": "No boundary"}).to_string().into()).boxed())
                    .unwrap());
            }

            let boundary = boundary.unwrap();
            let whole_body = match req.collect().await {
                Ok(body) => body.to_bytes(),
                Err(e) => {
                    eprintln!("Upload failed: Failed to read request body: {}", e);
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": "Failed to read body"}).to_string().into()).boxed())
                        .unwrap());
                }
            };

            eprintln!("Received upload: {} bytes, boundary: {}", whole_body.len(), boundary);

            match parse_multipart(&whole_body, &boundary) {
                Ok((filename, data)) => {
                    eprintln!("Parsed multipart: filename={}, size={}", filename, data.len());

                    let upload_folder = base_path.join("来自手机");
                    if let Err(e) = fs::create_dir_all(&upload_folder) {
                        eprintln!("Failed to create upload folder: {}", e);
                        return Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header("content-type", "application/json")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Full::new(serde_json::json!({"success": false, "error": "Failed to create folder"}).to_string().into()).boxed())
                            .unwrap());
                    }

                    let file_path = upload_folder.join(&filename);
                    if let Err(e) = fs::write(&file_path, &data) {
                        eprintln!("Failed to write file: {}", e);
                        return Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header("content-type", "application/json")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Full::new(serde_json::json!({"success": false, "error": "Failed to write file"}).to_string().into()).boxed())
                            .unwrap());
                    }

                    // Send event to frontend
                    let _ = app_handle.emit("file-received", serde_json::json!({
                        "filename": filename,
                        "size": data.len()
                    }));

                    let result = serde_json::json!({"success": true, "filename": filename});
                    Ok(Response::builder()
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(result.to_string().into()).boxed())
                        .unwrap())
                }
                Err(e) => {
                    eprintln!("Failed to parse multipart: {}", e);
                    Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": format!("Parse error: {}", e)}).to_string().into()).boxed())
                        .unwrap())
                }
            }
        }
        ("POST", "/api/upload/chunk") => {
            // Chunked upload endpoint
            let boundary = get_boundary(&req);

            if boundary.is_none() {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Full::new(serde_json::json!({"success": false, "error": "No boundary"}).to_string().into()).boxed())
                    .unwrap());
            }

            let boundary = boundary.unwrap();
            let whole_body = match req.collect().await {
                Ok(body) => body.to_bytes(),
                Err(_e) => {
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": "Failed to read body"}).to_string().into()).boxed())
                        .unwrap());
                }
            };

            match parse_chunked_multipart(&whole_body, &boundary) {
                Ok((filename, chunk_index, total_chunks, chunk_data)) => {
                    let upload_folder = base_path.join("chunks").join(&filename);
                    if let Err(e) = fs::create_dir_all(&upload_folder) {
                        return Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header("content-type", "application/json")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Full::new(serde_json::json!({"success": false, "error": format!("Failed to create folder: {}", e)}).to_string().into()).boxed())
                            .unwrap());
                    }

                    let chunk_path = upload_folder.join(format!("chunk_{}", chunk_index));
                    if let Err(e) = fs::write(&chunk_path, &chunk_data) {
                        return Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header("content-type", "application/json")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Full::new(serde_json::json!({"success": false, "error": format!("Failed to write chunk: {}", e)}).to_string().into()).boxed())
                            .unwrap());
                    }

                    let result = serde_json::json!({
                        "success": true,
                        "chunk": chunk_index,
                        "total": total_chunks
                    });
                    Ok(Response::builder()
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(result.to_string().into()).boxed())
                        .unwrap())
                }
                Err(e) => {
                    eprintln!("Failed to parse chunked upload: {}", e);
                    Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": format!("Parse error: {}", e)}).to_string().into()).boxed())
                        .unwrap())
                }
            }
        }
        ("POST", "/api/upload/complete") => {
            // Complete chunked upload and merge chunks
            let whole_body = match req.collect().await {
                Ok(body) => body.to_bytes(),
                Err(_) => {
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": "Failed to read body"}).to_string().into()).boxed())
                        .unwrap());
                }
            };

            let params: serde_json::Value = serde_json::from_slice(&whole_body).unwrap_or(serde_json::json!({}));
            let filename = params.get("filename").and_then(|v| v.as_str());
            let total_chunks = params.get("totalChunks").and_then(|v| v.as_u64());

            if let (Some(filename), Some(total_chunks)) = (filename, total_chunks) {
                let chunks_folder = base_path.join("chunks").join(filename);
                let upload_folder = base_path.join("来自手机");

                if let Err(_e) = fs::create_dir_all(&upload_folder) {
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": "Failed to create folder"}).to_string().into()).boxed())
                        .unwrap());
                }

                let final_path = upload_folder.join(filename);

                // Merge all chunks
                if let Ok(mut file) = fs::File::create(&final_path) {
                    let mut merged_size = 0u64;
                    for chunk_index in 0..total_chunks {
                        let chunk_path = chunks_folder.join(format!("chunk_{}", chunk_index));
                        if let Ok(chunk_data) = fs::read(&chunk_path) {
                            if file.write_all(&chunk_data).is_ok() {
                                merged_size += chunk_data.len() as u64;
                            }
                        }
                    }

                    // Clean up chunks folder
                    let _ = fs::remove_dir_all(&chunks_folder);

                    // Send event to frontend
                    let _ = app_handle.emit("file-received", serde_json::json!({
                        "filename": filename,
                        "size": merged_size
                    }));

                    let result = serde_json::json!({"success": true, "filename": filename, "size": merged_size});
                    Ok(Response::builder()
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(result.to_string().into()).boxed())
                        .unwrap())
                } else {
                    Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("content-type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(serde_json::json!({"success": false, "error": "Failed to create final file"}).to_string().into()).boxed())
                        .unwrap())
                }
            } else {
                Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Full::new(serde_json::json!({"success": false, "error": "Missing parameters"}).to_string().into()).boxed())
                    .unwrap())
            }
        }
        ("OPTIONS", _) => {
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                .header("Access-Control-Allow-Headers", "Content-Type")
                .body(Full::new("".into()).boxed())
                .unwrap())
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new("Not found".into()).boxed())
            .unwrap()),
    }
}

fn list_files(dir: &PathBuf) -> Vec<FileEntry> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') {
                let metadata = entry.metadata().unwrap();
                files.push(FileEntry {
                    name,
                    is_dir: metadata.is_dir(),
                    size: metadata.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.cmp(&b.name)
        }
    });
    files
}

fn get_boundary(req: &Request<Incoming>) -> Option<String> {
    let content_type = req.headers().get("content-type")?.to_str().ok()?;
    let boundary_start = content_type.find("boundary=")?;
    let boundary = &content_type[boundary_start + 9..];
    Some(boundary.to_string())
}

fn parse_multipart(data: &[u8], boundary: &str) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    let boundary_str = format!("--{}", boundary);
    let boundary_end_str = format!("--{}--", boundary);
    let boundary_bytes = boundary_str.as_bytes();
    let boundary_end = boundary_end_str.as_bytes();

    // Split by boundary
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut search_start = 0;

    while search_start < data.len() {
        // Find boundary
        let boundary_pos = data[search_start..]
            .windows(boundary_bytes.len())
            .position(|w| w == boundary_bytes)
            .ok_or("Boundary not found")?;

        let absolute_pos = search_start + boundary_pos;

        // Skip boundary and CRLF
        let part_start = absolute_pos + boundary_bytes.len();
        if part_start >= data.len() {
            break;
        }

        // Skip CRLF after boundary
        let mut actual_start = part_start;
        if data[actual_start..].starts_with(b"\r\n") {
            actual_start += 2;
        } else if data[actual_start..].starts_with(b"\n") {
            actual_start += 1;
        }

        // Find end of this part
        let next_boundary = data[actual_start..]
            .windows(boundary_bytes.len())
            .position(|w| w == boundary_bytes || w == boundary_end);

        if let Some(rel_end) = next_boundary {
            let part_end = actual_start + rel_end;
            parts.push(&data[actual_start..part_end]);
            search_start = part_end;
        } else {
            // Last part
            parts.push(&data[actual_start..]);
            break;
        }
    }

    // Find the part with filename
    for part in parts {
        let part_str = String::from_utf8_lossy(part);

        if let Some(filename_start) = part_str.find("filename=\"") {
            let filename_part = &part_str[filename_start + 10..];
            if let Some(filename_end) = filename_part.find('"') {
                let filename = filename_part[..filename_end].to_string();

                // Find empty line between headers and body
                let headers_end = part
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .or_else(|| part.windows(2).position(|w| w == b"\n\n"));

                if let Some(header_end) = headers_end {
                    let body_start = if part[header_end..].starts_with(b"\r\n\r\n") {
                        header_end + 4
                    } else {
                        header_end + 2
                    };

                    let mut file_data = part[body_start..].to_vec();

                    // Remove trailing CRLF
                    while file_data.ends_with(&[b'\r']) || file_data.ends_with(&[b'\n']) {
                        file_data.pop();
                    }

                    if !file_data.is_empty() {
                        return Ok((filename, file_data));
                    }
                }
            }
        }
    }

    Err("No valid file part found".into())
}

fn parse_chunked_multipart(data: &[u8], boundary: &str) -> Result<(String, u64, u64, Vec<u8>), Box<dyn std::error::Error>> {
    let boundary_str = format!("--{}", boundary);
    let boundary_end_str = format!("--{}--", boundary);
    let boundary_bytes = boundary_str.as_bytes();
    let boundary_end = boundary_end_str.as_bytes();

    // Split by boundary
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut search_start = 0;

    while search_start < data.len() {
        let boundary_pos = data[search_start..]
            .windows(boundary_bytes.len())
            .position(|w| w == boundary_bytes)
            .ok_or("Boundary not found")?;

        let absolute_pos = search_start + boundary_pos;
        let part_start = absolute_pos + boundary_bytes.len();
        if part_start >= data.len() {
            break;
        }

        let mut actual_start = part_start;
        if data[actual_start..].starts_with(b"\r\n") {
            actual_start += 2;
        } else if data[actual_start..].starts_with(b"\n") {
            actual_start += 1;
        }

        let next_boundary = data[actual_start..]
            .windows(boundary_bytes.len())
            .position(|w| w == boundary_bytes || w == boundary_end);

        if let Some(rel_end) = next_boundary {
            let part_end = actual_start + rel_end;
            parts.push(&data[actual_start..part_end]);
            search_start = part_end;
        } else {
            parts.push(&data[actual_start..]);
            break;
        }
    }

    // Parse each part - FormData sends fields as separate parts
    let mut filename = None;
    let mut chunk_index = None;
    let mut total_chunks = None;
    let mut file_data = None;

    for part in parts {
        let _part_str = String::from_utf8_lossy(part);

        // Find headers end
        if let Some(headers_end) = part.windows(4).position(|w| w == b"\r\n\r\n")
            .or_else(|| part.windows(2).position(|w| w == b"\n\n")) {

            let headers = &part[..headers_end];
            let headers_str = String::from_utf8_lossy(headers);

            // Get body data
            let body_start = if part[headers_end..].starts_with(b"\r\n\r\n") {
                headers_end + 4
            } else {
                headers_end + 2
            };

            let mut body_data = part[body_start..].to_vec();
            while body_data.ends_with(&[b'\r']) || body_data.ends_with(&[b'\n']) {
                body_data.pop();
            }

            // Check if this is the file part (has filename in Content-Disposition)
            if headers_str.contains("filename=") {
                if let Some(filename_start) = headers_str.find("filename=\"") {
                    let filename_part = &headers_str[filename_start + 10..];
                    if let Some(filename_end) = filename_part.find('"') {
                        filename = Some(filename_part[..filename_end].to_string());
                        file_data = Some(body_data);
                    }
                }
            }
            // Check for filename field
            else if headers_str.contains("name=\"filename\"") && !body_data.is_empty() {
                filename = Some(String::from_utf8_lossy(&body_data).to_string());
            }
            // Check for chunkIndex field
            else if headers_str.contains("name=\"chunkIndex\"") && !body_data.is_empty() {
                if let Ok(idx) = String::from_utf8_lossy(&body_data).parse::<u64>() {
                    chunk_index = Some(idx);
                }
            }
            // Check for totalChunks field
            else if headers_str.contains("name=\"totalChunks\"") && !body_data.is_empty() {
                if let Ok(total) = String::from_utf8_lossy(&body_data).parse::<u64>() {
                    total_chunks = Some(total);
                }
            }
        }
    }

    match (filename.clone(), chunk_index, total_chunks, file_data.clone()) {
        (Some(filename), Some(chunk_index), Some(total_chunks), Some(file_data)) => {
            Ok((filename, chunk_index, total_chunks, file_data))
        }
        _ => {
            eprintln!("Parse error - filename={:?}, chunk_index={:?}, total_chunks={:?}, file_data_len={:?}",
                filename.as_ref(), chunk_index, total_chunks, file_data.as_ref().map(|d| d.len()));
            Err("Missing required fields".into())
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_handle = app.handle().clone();
            app.manage(AppState {
                shared_folder: Arc::new(Mutex::new(None)),
                server_handle: Arc::new(Mutex::new(None)),
                app_handle,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_server,
            stop_server,
            generate_qr_code
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
