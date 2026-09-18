use axum::{
    routing::{get, post},
    extract::Path,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::os::unix::fs::symlink;
use std::fs;
use std::path::Path as StdPath;
use tower_http::cors::CorsLayer;
use tracing::{info, error, warn};
use std::time::Duration;
use tokio::time::sleep;

const BASE_DIR: &str = "/home/uc/Sources/webspeak3";
const COMPOSE_FILE: &str = "docker-compose.yml";
const OVERRIDE_FILE: &str = "docker-compose.override.yml";
const VPN_CONTAINER: &str = "vpn_gateway";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    provider: String,
    ip: String,
    host_ip: String,
    vpn_ip: String,
    leak: bool,
}

#[derive(Serialize)]
struct RotationResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
struct LeakResponse {
    leak: bool,
    host_ip: String,
    vpn_ip: String,
}

async fn get_current_ip() -> String {
    let output = Command::new("docker")
        .args(["exec", VPN_CONTAINER, "cat", "/tmp/gluetun/ip"])
        .output();

    match output {
        Ok(out) => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ip.is_empty() {
                "Unknown".to_string()
            } else {
                ip
            }
        }
        Err(_) => "Unknown".to_string(),
    }
}

async fn get_host_ip() -> String {
    let output = Command::new("curl")
        .args(["-s", "https://ipinfo.io/ip"])
        .output();

    match output {
        Ok(out) => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ip.is_empty() {
                "Unknown".to_string()
            } else {
                ip
            }
        }
        Err(_) => "Unknown".to_string(),
    }
}

async fn get_status() -> Json<StatusResponse> {
    let host_ip = get_host_ip().await;
    let vpn_ip = get_current_ip().await;

    let compose_path = StdPath::new(BASE_DIR).join(COMPOSE_FILE);
    let provider = match fs::read_link(&compose_path) {
        Ok(target) => {
            let target_str = target.to_string_lossy();
            if target_str.contains("protonvpn") { "ProtonVPN".to_string() }
            else if target_str.contains("nordvpn") { "NordVPN".to_string() }
            else if target_str.contains("direct") { "Direct".to_string() }
            else { "Unknown".to_string() }
        }
        Err(_) => "Unknown".to_string(),
    };

    let is_direct = provider == "Direct";
    let ip = if is_direct { host_ip.clone() } else { vpn_ip.clone() };
    let leak = if is_direct {
        false
    } else {
        host_ip == "Unknown" || vpn_ip == "Unknown" || host_ip == vpn_ip
    };

    Json(StatusResponse { provider, ip, host_ip, vpn_ip, leak })
}

async fn verify_leak() -> Json<LeakResponse> {
    let compose_path = StdPath::new(BASE_DIR).join(COMPOSE_FILE);
    if let Ok(target) = fs::read_link(&compose_path) {
        if target.to_string_lossy().contains("direct") {
            return Json(LeakResponse { leak: false, host_ip: "".into(), vpn_ip: "".into() });
        }
    }

    let host_ip = get_host_ip().await;
    let vpn_ip = get_current_ip().await;

    let leak = host_ip == "Unknown" || vpn_ip == "Unknown" || host_ip == vpn_ip;

    Json(LeakResponse { leak, host_ip, vpn_ip })
}

async fn rotate_provider(Path(provider): Path<String>) -> Json<RotationResponse> {
    info!("Rotating network to provider: {}", provider);
    
    let provider_lower = provider.to_lowercase();
    let provider_suffix = match provider_lower.as_str() {
        "direct" => "direct",
        "protonvpn" => "protonvpn",
        "nordvpn" => "nordvpn",
        _ => {
            return Json(RotationResponse {
                success: false,
                message: format!("Unsupported provider: {}", provider),
            });
        }
    };

    // --- SMART CHECK ---
    let compose_path = StdPath::new(BASE_DIR).join(COMPOSE_FILE);
    if let Ok(target) = fs::read_link(&compose_path) {
        let target_str = target.to_string_lossy();
        if target_str.contains(provider_suffix) {
            info!("Already using provider {}, skipping restart.", provider);
            return Json(RotationResponse {
                success: true,
                message: format!("Already connected via {}", provider),
            });
        }
    }
    // --- END SMART CHECK ---

    let files_to_link = [
        (COMPOSE_FILE, format!("{}.{}", COMPOSE_FILE, provider_suffix)),
        (OVERRIDE_FILE, format!("{}.{}", OVERRIDE_FILE, provider_suffix)),
    ];

    for (dest, src) in files_to_link {
        let dest_path = StdPath::new(BASE_DIR).join(dest);
        let src_path = StdPath::new(BASE_DIR).join(&src);

        if let Err(e) = fs::remove_file(&dest_path) {
            warn!("Could not remove {}: {}", dest, e);
        }

        if let Err(e) = symlink(&src_path, &dest_path) {
            error!("Failed to symlink {} to {}: {}", src, dest, e);
            return Json(RotationResponse {
                success: false,
                message: format!("Symlink error: {}", e),
            });
        }
    }

    info!("Executing docker compose down...");
    let down_status = Command::new("docker")
        .args(["compose", "down", "--remove-orphans"])
        .current_dir(BASE_DIR)
        .status();

    if let Err(e) = down_status {
        error!("Failed to run docker compose down: {}", e);
        return Json(RotationResponse { success: false, message: e.to_string() });
    }

    info!("Executing docker compose up...");
    let up_status = Command::new("docker")
        .args(["compose", "up", "-d", "--force-recreate"])
        .current_dir(BASE_DIR)
        .status();

    if let Err(e) = up_status {
        error!("Failed to run docker compose up: {}", e);
        return Json(RotationResponse { success: false, message: e.to_string() });
    }

    sleep(Duration::from_secs(15)).await;

    let mut attempts = 0;
    let max_attempts = 20;
    let last_ip = get_current_ip().await;
    
    info!("Verifying IP change via Gluetun. Starting IP: {}", last_ip);

    loop {
        attempts += 1;
        sleep(Duration::from_secs(5)).await;
        
        let current_ip = get_current_ip().await;
        info!("Attempt {}: Container IP is {}", attempts, current_ip);

        if current_ip != last_ip && current_ip != "Unknown" {
            info!("IP successfully changed to {}", current_ip);
            return Json(RotationResponse {
                success: true,
                message: format!("Switched to {} and verified IP: {}", provider, current_ip),
            });
        }

        if attempts >= max_attempts {
            error!("IP verification timed out. Final IP: {}", current_ip);
            return Json(RotationResponse {
                success: false,
                message: "Network rotation succeeded but IP verification timed out. Please check your VPN credentials.".to_string(),
            });
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/status", get(get_status))
        .route("/verify-leak", get(verify_leak))
        .route("/rotate/:provider", post(rotate_provider))
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Network Manager Daemon listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
