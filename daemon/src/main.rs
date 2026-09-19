use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    fs::File,
    io::Read,
    os::unix::{fs::symlink, prelude::PermissionsExt},
    path::{Path as StdPath, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Mutex, time::sleep};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

const BASE_DIR: &str = "/home/uc/Sources/webspeak3";
const COMPOSE_FILE: &str = "docker-compose.yml";
const OVERRIDE_FILE: &str = "docker-compose.override.yml";
const VPN_CONTAINER: &str = "vpn_gateway";
const RUNTIME_DIR: &str = "/run/webspeak3";
const PROFILE_OVERLAY_FILE: &str = "/run/webspeak3/profile-selection.override.yml";
const ROTATION_STATE_FILE: &str = "/var/lib/webspeak3/profile-rotation-state.json";
const MAX_PROFILE_BYTES: u64 = 128 * 1024;
const STARTUP_DELAY: Duration = Duration::from_secs(15);
const VERIFICATION_ATTEMPTS: u8 = 20;
const VERIFICATION_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct AppState {
    rotation_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Provider {
    Direct,
    NordVpn,
    ProtonVpn,
}

impl Provider {
    fn from_request(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "direct" => Some(Self::Direct),
            "nordvpn" => Some(Self::NordVpn),
            "protonvpn" => Some(Self::ProtonVpn),
            _ => None,
        }
    }

    fn compose_suffix(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::NordVpn => "nordvpn",
            Self::ProtonVpn => "protonvpn",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::NordVpn => "NordVPN",
            Self::ProtonVpn => "ProtonVPN",
        }
    }

    fn profile_directory_key(self) -> Option<&'static str> {
        match self {
            Self::NordVpn => Some("NORDVPN_PROFILE_DIRECTORY"),
            Self::ProtonVpn => Some("PROTONVPN_PROFILE_DIRECTORY"),
            Self::Direct => None,
        }
    }

    fn profile_extensions_key(self) -> Option<&'static str> {
        match self {
            Self::NordVpn => Some("NORDVPN_PROFILE_EXTENSIONS"),
            Self::ProtonVpn => Some("PROTONVPN_PROFILE_EXTENSIONS"),
            Self::Direct => None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    provider: String,
    profile: Option<String>,
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

#[derive(Clone)]
struct ProfileSettings {
    directory: PathBuf,
    extensions: HashSet<String>,
}

#[derive(Clone)]
struct ValidatedProfile {
    basename: String,
    path: PathBuf,
    // NordVPN selection uses Gluetun's SERVER_HOSTNAMES filter. ProtonVPN
    // instead bind-mounts the complete validated profile read-only.
    endpoint_hostname: Option<String>,
}

#[derive(Clone)]
struct ProfileSelection {
    profile: ValidatedProfile,
    proposed_state: RotationState,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ProviderRotationState {
    // The profile that the most recently successful rotation activated.
    active_profile: Option<String>,
    // The most recent successful selection. It is kept separate so a newly
    // shuffled cycle can avoid selecting the same profile first.
    last_profile: Option<String>,
    // Profiles not yet selected in the current shuffled cycle.
    remaining_profiles: Vec<String>,
    // The validated pool known when this cycle began. This lets discovery add
    // newly introduced files without putting previously used profiles back
    // into the current cycle.
    cycle_profiles: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct RotationState {
    nordvpn: ProviderRotationState,
    protonvpn: ProviderRotationState,
}

impl RotationState {
    fn provider_state(&self, provider: Provider) -> Option<&ProviderRotationState> {
        match provider {
            Provider::NordVpn => Some(&self.nordvpn),
            Provider::ProtonVpn => Some(&self.protonvpn),
            Provider::Direct => None,
        }
    }

    fn provider_state_mut(&mut self, provider: Provider) -> Option<&mut ProviderRotationState> {
        match provider {
            Provider::NordVpn => Some(&mut self.nordvpn),
            Provider::ProtonVpn => Some(&mut self.protonvpn),
            Provider::Direct => None,
        }
    }
}

async fn get_current_ip() -> String {
    let output = Command::new("docker")
        .args(["exec", VPN_CONTAINER, "cat", "/tmp/gluetun/ip"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ip.is_empty() {
                "Unknown".to_string()
            } else {
                ip
            }
        }
        _ => "Unknown".to_string(),
    }
}

async fn get_host_ip() -> String {
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", "10", "https://ipinfo.io/ip"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ip.is_empty() {
                "Unknown".to_string()
            } else {
                ip
            }
        }
        _ => "Unknown".to_string(),
    }
}

fn current_provider() -> Option<Provider> {
    let compose_path = StdPath::new(BASE_DIR).join(COMPOSE_FILE);
    let target = fs::read_link(compose_path).ok()?;
    let target = target.to_string_lossy();

    if target.contains("protonvpn") {
        Some(Provider::ProtonVpn)
    } else if target.contains("nordvpn") {
        Some(Provider::NordVpn)
    } else if target.contains("direct") {
        Some(Provider::Direct)
    } else {
        None
    }
}

fn active_profile(provider: Provider) -> Option<String> {
    if provider == Provider::Direct {
        return None;
    }

    // Report a profile only when the saved basename still resolves to a
    // currently validated file. This avoids displaying stale state after a
    // profile is removed or becomes invalid and a normal provider switch
    // falls back to the provider's static Compose configuration.
    load_active_profile(provider)
        .ok()
        .flatten()
        .map(|profile| profile.basename)
}

fn safe_basename(value: &str) -> Option<String> {
    let path = StdPath::new(value);
    let name = path.file_name()?.to_str()?;
    if name == value && !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

fn is_leak(host_ip: &str, vpn_ip: &str) -> bool {
    host_ip == "Unknown" || vpn_ip == "Unknown" || host_ip == vpn_ip
}

async fn get_status() -> Json<StatusResponse> {
    let host_ip = get_host_ip().await;
    let vpn_ip = get_current_ip().await;
    let provider = current_provider();
    let is_direct = provider == Some(Provider::Direct);
    let provider_name = provider
        .map(Provider::display_name)
        .unwrap_or("Unknown")
        .to_string();
    let ip = if is_direct {
        host_ip.clone()
    } else {
        vpn_ip.clone()
    };
    let leak = if is_direct {
        false
    } else {
        is_leak(&host_ip, &vpn_ip)
    };
    let profile = provider.and_then(active_profile);

    Json(StatusResponse {
        provider: provider_name,
        profile,
        ip,
        host_ip,
        vpn_ip,
        leak,
    })
}

async fn verify_leak() -> Json<LeakResponse> {
    if current_provider() == Some(Provider::Direct) {
        return Json(LeakResponse {
            leak: false,
            host_ip: "".into(),
            vpn_ip: "".into(),
        });
    }

    let host_ip = get_host_ip().await;
    let vpn_ip = get_current_ip().await;
    let leak = is_leak(&host_ip, &vpn_ip);

    Json(LeakResponse {
        leak,
        host_ip,
        vpn_ip,
    })
}

async fn rotate_provider(
    State(app_state): State<AppState>,
    Path(provider): Path<String>,
) -> Json<RotationResponse> {
    let Some(provider) = Provider::from_request(&provider) else {
        return unsupported_provider_response(&provider);
    };

    let _rotation_guard = app_state.rotation_lock.lock().await;
    rotate_to_provider(provider, false).await
}

async fn rotate_profile(
    State(app_state): State<AppState>,
    Path(provider): Path<String>,
) -> Json<RotationResponse> {
    let Some(provider) = Provider::from_request(&provider) else {
        return unsupported_provider_response(&provider);
    };

    if provider == Provider::Direct {
        return Json(RotationResponse {
            success: false,
            message: "Direct mode has no VPN profiles to rotate.".to_string(),
        });
    }

    let _rotation_guard = app_state.rotation_lock.lock().await;
    rotate_to_provider(provider, true).await
}

fn unsupported_provider_response(provider: &str) -> Json<RotationResponse> {
    Json(RotationResponse {
        success: false,
        message: format!("Unsupported provider: {provider}"),
    })
}

// The Gateway calls POST /rotate/{provider} before each TeamSpeak connection.
// Keep that endpoint idempotent for an already-selected provider. It never
// advances the profile shuffle bag: only POST /rotate/{provider}/profile is a
// manual profile rotation. A normal provider switch reuses a prior manual
// selection when one exists, otherwise it keeps the provider's static Compose
// configuration.
async fn rotate_to_provider(
    provider: Provider,
    force_profile_rotation: bool,
) -> Json<RotationResponse> {
    let active_provider = current_provider();

    if !force_profile_rotation && active_provider == Some(provider) {
        info!(
            "Provider {} is already active; preserving its profile.",
            provider.display_name()
        );
        return Json(RotationResponse {
            success: true,
            message: format!("Already connected via {}", provider.display_name()),
        });
    }

    if provider == Provider::Direct {
        return recreate_direct_stack(active_provider).await;
    }

    if !force_profile_rotation {
        // A normal Gateway pre-connect request must never advance the shuffle
        // bag. If this provider has a profile selected by a prior *manual*
        // rotation, reuse it after a provider switch. Otherwise retain the
        // legacy static Gluetun configuration from the provider Compose file.
        return activate_existing_provider_selection(provider, active_provider).await;
    }

    let selection = match choose_next_profile(provider) {
        Ok(selection) => selection,
        Err(reason) => {
            warn!(
                "Could not select a {} profile: {}",
                provider.display_name(),
                reason
            );
            return Json(RotationResponse {
                success: false,
                message: format!(
                    "Could not select a {} profile: {reason}",
                    provider.display_name()
                ),
            });
        }
    };

    info!(
        "Selected next {} profile {} for manual rotation.",
        provider.display_name(),
        selection.profile.basename
    );

    let overlay = match write_profile_overlay(provider, &selection.profile) {
        Ok(path) => path,
        Err(reason) => {
            error!(
                "Could not prepare the runtime profile selection: {}",
                reason
            );
            return Json(RotationResponse {
                success: false,
                message: format!("Could not prepare profile selection: {reason}"),
            });
        }
    };

    if let Err(reason) = recreate_stack(provider, active_provider, Some(&overlay)).await {
        error!(
            "Failed to activate {} profile: {}",
            provider.display_name(),
            reason
        );
        return Json(RotationResponse {
            success: false,
            message: format!("Network rotation failed: {reason}"),
        });
    }

    if let Err(reason) = wait_for_safe_vpn().await {
        error!(
            "{} profile failed post-start leak verification: {}",
            provider.display_name(),
            reason
        );
        return Json(RotationResponse {
            success: false,
            message: format!("Network rotation failed safety verification: {reason}"),
        });
    }

    if let Err(reason) = save_rotation_state(&selection.proposed_state) {
        // The selected profile is live and leak-free, so do not report a failed
        // network rotation. The warning makes the lost no-repeat history clear.
        error!(
            "Profile is active but rotation state could not be saved: {}",
            reason
        );
        return Json(RotationResponse {
            success: true,
            message: format!(
                "Switched to {} using profile {}, but could not persist rotation history.",
                provider.display_name(),
                selection.profile.basename
            ),
        });
    }

    Json(RotationResponse {
        success: true,
        message: format!(
            "Switched to {} using profile {} and verified leak protection.",
            provider.display_name(),
            selection.profile.basename
        ),
    })
}

async fn activate_existing_provider_selection(
    provider: Provider,
    previous_provider: Option<Provider>,
) -> Json<RotationResponse> {
    let profile = match load_active_profile(provider) {
        Ok(profile) => profile,
        Err(reason) => {
            warn!(
                "Could not reuse the saved {} profile; using the provider's static Compose configuration: {}",
                provider.display_name(),
                reason
            );
            None
        }
    };

    let overlay = match profile.as_ref() {
        Some(profile) => match write_profile_overlay(provider, profile) {
            Ok(path) => Some(path),
            Err(reason) => {
                return Json(RotationResponse {
                    success: false,
                    message: format!("Could not prepare saved profile selection: {reason}"),
                });
            }
        },
        None => None,
    };

    if let Err(reason) = recreate_stack(provider, previous_provider, overlay.as_deref()).await {
        error!(
            "Failed to switch to {}: {}",
            provider.display_name(),
            reason
        );
        return Json(RotationResponse {
            success: false,
            message: format!("Network rotation failed: {reason}"),
        });
    }

    if let Err(reason) = wait_for_safe_vpn().await {
        error!(
            "{} failed post-start leak verification: {}",
            provider.display_name(),
            reason
        );
        return Json(RotationResponse {
            success: false,
            message: format!("Network rotation failed safety verification: {reason}"),
        });
    }

    let message = match profile {
        Some(profile) => format!(
            "Switched to {} using the existing profile {} and verified leak protection.",
            provider.display_name(),
            profile.basename
        ),
        None => format!(
            "Switched to {} using its static Compose configuration and verified leak protection.",
            provider.display_name()
        ),
    };
    Json(RotationResponse {
        success: true,
        message,
    })
}

fn load_active_profile(provider: Provider) -> Result<Option<ValidatedProfile>, String> {
    let Some(active_basename) = load_rotation_state()?
        .provider_state(provider)
        .and_then(|state| state.active_profile.as_deref())
        .and_then(safe_basename)
    else {
        return Ok(None);
    };

    let settings = profile_settings(provider)?;
    let profile = discover_profiles(provider, &settings)?
        .into_iter()
        .find(|profile| profile.basename == active_basename);
    Ok(profile)
}

async fn recreate_direct_stack(previous_provider: Option<Provider>) -> Json<RotationResponse> {
    match recreate_stack(Provider::Direct, previous_provider, None).await {
        Ok(()) => Json(RotationResponse {
            success: true,
            message: "Switched to Direct".to_string(),
        }),
        Err(reason) => {
            error!("Failed to switch to Direct: {}", reason);
            Json(RotationResponse {
                success: false,
                message: format!("Network rotation failed: {reason}"),
            })
        }
    }
}

async fn recreate_stack(
    provider: Provider,
    previous_provider: Option<Provider>,
    profile_overlay: Option<&StdPath>,
) -> Result<(), String> {
    info!(
        "Stopping the current Compose stack before selecting {}.",
        provider.display_name()
    );
    run_compose_down()?;

    // Preserve the old symlink targets so a failed provider switch can be
    // retried with the prior source selection rather than becoming a false
    // idempotent success on the next Gateway request.
    let old_compose_target = read_link_target(COMPOSE_FILE);
    let old_override_target = read_link_target(OVERRIDE_FILE);

    if let Err(reason) = set_provider_symlinks(provider) {
        restore_symlink(COMPOSE_FILE, old_compose_target.as_deref());
        restore_symlink(OVERRIDE_FILE, old_override_target.as_deref());
        return Err(reason);
    }

    if let Err(reason) = run_compose_up(profile_overlay) {
        if previous_provider != Some(provider) {
            restore_symlink(COMPOSE_FILE, old_compose_target.as_deref());
            restore_symlink(OVERRIDE_FILE, old_override_target.as_deref());
        }
        return Err(reason);
    }

    Ok(())
}

fn read_link_target(filename: &str) -> Option<PathBuf> {
    fs::read_link(StdPath::new(BASE_DIR).join(filename)).ok()
}

fn restore_symlink(filename: &str, target: Option<&StdPath>) {
    let Some(target) = target else {
        return;
    };
    let path = StdPath::new(BASE_DIR).join(filename);
    if let Err(error) = fs::remove_file(&path) {
        warn!(
            "Could not remove {} while restoring provider symlink: {}",
            filename, error
        );
        return;
    }
    if let Err(error) = symlink(target, &path) {
        error!("Could not restore {} provider symlink: {}", filename, error);
    }
}

fn set_provider_symlinks(provider: Provider) -> Result<(), String> {
    let files_to_link = [
        (
            COMPOSE_FILE,
            format!("{}.{}", COMPOSE_FILE, provider.compose_suffix()),
        ),
        (
            OVERRIDE_FILE,
            format!("{}.{}", OVERRIDE_FILE, provider.compose_suffix()),
        ),
    ];

    for (destination, source) in files_to_link {
        let destination_path = StdPath::new(BASE_DIR).join(destination);
        let source_path = StdPath::new(BASE_DIR).join(&source);

        if !source_path.is_file() {
            return Err(format!("Compose source file is missing: {source}"));
        }
        if let Err(error) = fs::remove_file(&destination_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("Could not replace {destination}: {error}"));
            }
        }
        symlink(&source_path, &destination_path)
            .map_err(|error| format!("Could not link {destination} to {source}: {error}"))?;
    }

    Ok(())
}

fn run_compose_down() -> Result<(), String> {
    let status = Command::new("docker")
        .args(["compose", "down", "--remove-orphans"])
        .current_dir(BASE_DIR)
        .status()
        .map_err(|error| format!("Could not start docker compose down: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("docker compose down exited with status {status}"))
    }
}

fn run_compose_up(profile_overlay: Option<&StdPath>) -> Result<(), String> {
    let mut command = Command::new("docker");
    command.current_dir(BASE_DIR);
    command.arg("compose");

    if let Some(profile_overlay) = profile_overlay {
        command
            .arg("-f")
            .arg(COMPOSE_FILE)
            .arg("-f")
            .arg(OVERRIDE_FILE)
            .arg("-f")
            .arg(profile_overlay);
    }

    command.args(["up", "-d", "--force-recreate"]);
    let status = command
        .status()
        .map_err(|error| format!("Could not start docker compose up: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("docker compose up exited with status {status}"))
    }
}

async fn wait_for_safe_vpn() -> Result<(), String> {
    sleep(STARTUP_DELAY).await;

    for attempt in 1..=VERIFICATION_ATTEMPTS {
        let host_ip = get_host_ip().await;
        let vpn_ip = get_current_ip().await;

        if !is_leak(&host_ip, &vpn_ip) {
            info!(
                "VPN stack passed leak verification after {} attempt(s).",
                attempt
            );
            return Ok(());
        }

        if attempt < VERIFICATION_ATTEMPTS {
            sleep(VERIFICATION_DELAY).await;
        }
    }

    Err("host/VPN egress could not be verified as distinct".to_string())
}

fn choose_next_profile(provider: Provider) -> Result<ProfileSelection, String> {
    let settings = profile_settings(provider)?;
    let profiles = discover_profiles(provider, &settings)?;
    if profiles.is_empty() {
        return Err("no valid WireGuard profiles were found".to_string());
    }

    let mut state = load_rotation_state()?;
    let provider_state = state
        .provider_state_mut(provider)
        .expect("VPN provider always has rotation state");
    let valid_names: HashSet<String> = profiles
        .iter()
        .map(|profile| profile.basename.clone())
        .collect();

    if provider_state
        .active_profile
        .as_ref()
        .is_some_and(|profile| !valid_names.contains(profile))
    {
        provider_state.active_profile = None;
    }
    if provider_state
        .last_profile
        .as_ref()
        .is_some_and(|profile| !valid_names.contains(profile))
    {
        provider_state.last_profile = None;
    }

    let mut seen_cycle_profiles = HashSet::new();
    provider_state.cycle_profiles.retain(|profile| {
        valid_names.contains(profile) && seen_cycle_profiles.insert(profile.clone())
    });

    let active_profile = provider_state.active_profile.clone();
    let mut seen_remaining_profiles = HashSet::new();
    provider_state.remaining_profiles.retain(|profile| {
        Some(profile) != active_profile.as_ref()
            && valid_names.contains(profile)
            && seen_remaining_profiles.insert(profile.clone())
    });

    // A file added after a cycle starts is eligible in this cycle. A profile
    // already in cycle_profiles has been used or remains pending, so it must
    // not be re-added until the cycle is exhausted.
    let mut reshuffle = false;
    for profile in &profiles {
        if !provider_state.cycle_profiles.contains(&profile.basename) {
            provider_state.cycle_profiles.push(profile.basename.clone());
            if Some(&profile.basename) != active_profile.as_ref()
                && !provider_state
                    .remaining_profiles
                    .contains(&profile.basename)
            {
                provider_state
                    .remaining_profiles
                    .push(profile.basename.clone());
            }
            reshuffle = true;
        }
    }

    // At the end of a cycle, make every current profile eligible again. The
    // following shuffle is adjusted so the profile just used cannot be chosen
    // immediately again when another profile exists.
    if provider_state.remaining_profiles.is_empty() {
        provider_state.cycle_profiles = profiles
            .iter()
            .map(|profile| profile.basename.clone())
            .collect();
        provider_state.remaining_profiles = provider_state.cycle_profiles.clone();
        reshuffle = true;
    }

    if reshuffle {
        secure_shuffle(&mut provider_state.remaining_profiles)?;
        if provider_state.remaining_profiles.len() > 1 {
            if let Some(last_profile) = provider_state.last_profile.as_ref() {
                let final_index = provider_state.remaining_profiles.len() - 1;
                if provider_state.remaining_profiles[final_index] == *last_profile {
                    let alternate_index = provider_state
                        .remaining_profiles
                        .iter()
                        .position(|profile| profile != last_profile)
                        .expect("a multi-profile pool always has an alternative");
                    provider_state
                        .remaining_profiles
                        .swap(final_index, alternate_index);
                }
            }
        }
    }

    let selected_name = provider_state
        .remaining_profiles
        .pop()
        .ok_or_else(|| "no profile remained after profile-pool reconciliation".to_string())?;
    let profile = profiles
        .iter()
        .find(|profile| profile.basename == selected_name)
        .cloned()
        .ok_or_else(|| "selected profile disappeared during discovery".to_string())?;

    provider_state.active_profile = Some(selected_name.clone());
    provider_state.last_profile = Some(selected_name);

    Ok(ProfileSelection {
        profile,
        proposed_state: state,
    })
}

fn profile_settings(provider: Provider) -> Result<ProfileSettings, String> {
    let directory_key = provider
        .profile_directory_key()
        .ok_or_else(|| "Direct mode has no profile settings".to_string())?;
    let extensions_key = provider
        .profile_extensions_key()
        .ok_or_else(|| "Direct mode has no profile settings".to_string())?;
    // Do not deserialize or retain the credential values in .env. The daemon
    // reads only the two non-secret profile-discovery settings it needs.
    let env_path = StdPath::new(BASE_DIR).join(".env");
    let directory = read_dotenv_setting(&env_path, directory_key)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{directory_key} is not configured in .env"))?;
    let raw_extensions = read_dotenv_setting(&env_path, extensions_key)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{extensions_key} is not configured in .env"))?;

    let mut extensions = HashSet::new();
    for extension in raw_extensions
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let extension = extension.to_ascii_lowercase();
        if !is_valid_extension(&extension) {
            return Err(format!("{extensions_key} contains an invalid extension"));
        }
        extensions.insert(extension);
    }
    if extensions.is_empty() {
        return Err(format!("{extensions_key} has no usable extensions"));
    }

    Ok(ProfileSettings {
        directory: PathBuf::from(directory),
        extensions,
    })
}

fn read_dotenv_setting(path: &StdPath, requested_key: &str) -> Result<Option<String>, String> {
    let contents =
        fs::read_to_string(path).map_err(|error| format!("could not read .env: {error}"))?;

    let mut selected_value = None;
    for original_line in contents.lines() {
        let line = original_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == requested_key {
            selected_value = Some(unquote_env_value(raw_value.trim()));
        }
    }

    Ok(selected_value)
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn is_valid_extension(extension: &str) -> bool {
    extension.starts_with('.')
        && extension.len() > 1
        && extension[1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
}

fn discover_profiles(
    provider: Provider,
    settings: &ProfileSettings,
) -> Result<Vec<ValidatedProfile>, String> {
    let directory = fs::canonicalize(&settings.directory)
        .map_err(|error| format!("profile directory is unavailable: {error}"))?;
    if !directory.is_dir() {
        return Err("configured profile directory is not a directory".to_string());
    }

    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("could not enumerate profile directory: {error}"))?;
    let mut profiles = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!("Skipping unreadable VPN profile directory entry: {}", error);
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warn!("Skipping inaccessible VPN profile entry: {}", error);
                continue;
            }
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.len() > MAX_PROFILE_BYTES {
            warn!(
                "Skipping oversized {} profile {}.",
                provider.display_name(),
                display_filename(&path)
            );
            continue;
        }

        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if !settings
            .extensions
            .contains(&format!(".{}", extension.to_ascii_lowercase()))
        {
            continue;
        }

        match validate_wireguard_profile(provider, &path) {
            Ok(profile) => profiles.push(profile),
            Err(reason) => warn!(
                "Skipping invalid {} profile {}: {}",
                provider.display_name(),
                display_filename(&path),
                reason
            ),
        }
    }

    profiles.sort_by(|left, right| left.basename.cmp(&right.basename));
    Ok(profiles)
}

fn display_filename(path: &StdPath) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<non-UTF-8 filename>")
        .to_string()
}

fn validate_wireguard_profile(
    provider: Provider,
    path: &StdPath,
) -> Result<ValidatedProfile, String> {
    let contents =
        fs::read_to_string(path).map_err(|_| "profile is not valid UTF-8 text".to_string())?;
    let mut interface_seen = false;
    let mut peer_seen = false;
    let mut current_section: Option<&str> = None;
    let mut values = HashMap::<(&str, String), String>::new();

    for (index, original_line) in contents.lines().enumerate() {
        let line = original_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_section = match line {
                "[Interface]" if !interface_seen => {
                    interface_seen = true;
                    Some("Interface")
                }
                "[Peer]" if !peer_seen => {
                    peer_seen = true;
                    Some("Peer")
                }
                "[Interface]" | "[Peer]" => return Err("duplicate section".to_string()),
                _ => return Err(format!("unsupported section on line {}", index + 1)),
            };
            continue;
        }

        let section = current_section
            .ok_or_else(|| format!("setting before a section on line {}", index + 1))?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed setting on line {}", index + 1))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return Err(format!("empty setting on line {}", index + 1));
        }
        let composite_key = (section, key.to_string());
        if values.insert(composite_key, value.to_string()).is_some() {
            return Err(format!("duplicate setting on line {}", index + 1));
        }
    }

    if !interface_seen || !peer_seen {
        return Err("[Interface] and [Peer] sections are both required".to_string());
    }

    let private_key = required_profile_field(&values, "Interface", "PrivateKey")?;
    let interface_address = required_profile_field(&values, "Interface", "Address")?;
    let public_key = required_profile_field(&values, "Peer", "PublicKey")?;
    let allowed_ips = required_profile_field(&values, "Peer", "AllowedIPs")?;
    let endpoint = required_profile_field(&values, "Peer", "Endpoint")?;

    if !is_wireguard_key(private_key) || !is_wireguard_key(public_key) {
        return Err("WireGuard key is not a 32-byte base64 key".to_string());
    }
    if !is_cidr_list(interface_address) || !is_cidr_list(allowed_ips) {
        return Err("interface address or allowed IP range is invalid".to_string());
    }
    let endpoint_host = parse_endpoint_host(endpoint)?;
    let endpoint_hostname = match provider {
        Provider::NordVpn => {
            if endpoint_host.parse::<std::net::IpAddr>().is_ok() {
                return Err(
                    "NordVPN endpoint must be a hostname for Gluetun server selection".to_string(),
                );
            }
            Some(endpoint_host)
        }
        Provider::ProtonVpn | Provider::Direct => None,
    };

    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(safe_basename)
        .ok_or_else(|| "profile filename is not a safe UTF-8 basename".to_string())?;

    Ok(ValidatedProfile {
        basename,
        path: path.to_path_buf(),
        endpoint_hostname,
    })
}

fn required_profile_field<'a>(
    values: &'a HashMap<(&str, String), String>,
    section: &'a str,
    key: &str,
) -> Result<&'a str, String> {
    values
        .get(&(section, key.to_string()))
        .map(String::as_str)
        .ok_or_else(|| format!("missing required {section}.{key}"))
}

fn is_wireguard_key(value: &str) -> bool {
    // A 32-byte key encoded with standard base64 is exactly 44 bytes: 43
    // base64 symbols followed by one padding character.
    value.len() == 44
        && value.ends_with('=')
        && value[..43]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
}

fn is_cidr_list(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .all(|cidr| !cidr.is_empty() && is_cidr(cidr))
}

fn is_cidr(value: &str) -> bool {
    let Some((ip, prefix)) = value.rsplit_once('/') else {
        return false;
    };
    let Ok(ip) = ip.trim().parse::<std::net::IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.trim().parse::<u8>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(_) => prefix <= 32,
        std::net::IpAddr::V6(_) => prefix <= 128,
    }
}

fn parse_endpoint_host(value: &str) -> Result<String, String> {
    let value = value.trim();
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| "endpoint must use host:port syntax".to_string())?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("bracketed endpoint host is not an IPv6 address".to_string());
        }
        (host, port)
    } else {
        value
            .rsplit_once(':')
            .ok_or_else(|| "endpoint must use host:port syntax".to_string())?
    };

    if host.is_empty() || port.parse::<u16>().ok().filter(|port| *port > 0).is_none() {
        return Err("endpoint host or port is invalid".to_string());
    }
    if host.parse::<std::net::IpAddr>().is_err() && !is_valid_hostname(host) {
        return Err("endpoint hostname is invalid".to_string());
    }

    Ok(host.to_ascii_lowercase())
}

fn is_valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn secure_shuffle<T>(items: &mut [T]) -> Result<(), String> {
    if items.len() < 2 {
        return Ok(());
    }

    let mut random = File::open("/dev/urandom")
        .map_err(|error| format!("could not open system random source: {error}"))?;
    for index in (1..items.len()).rev() {
        let selected = random_index(&mut random, index + 1)?;
        items.swap(index, selected);
    }
    Ok(())
}

fn random_index(random: &mut File, upper_bound: usize) -> Result<usize, String> {
    let upper_bound =
        u64::try_from(upper_bound).map_err(|_| "profile pool is too large".to_string())?;
    let acceptance_limit = u64::MAX - (u64::MAX % upper_bound);

    loop {
        let mut bytes = [0_u8; 8];
        random
            .read_exact(&mut bytes)
            .map_err(|error| format!("could not read system random source: {error}"))?;
        let value = u64::from_le_bytes(bytes);
        if value < acceptance_limit {
            return usize::try_from(value % upper_bound)
                .map_err(|_| "random profile index does not fit platform size".to_string());
        }
    }
}

fn load_rotation_state() -> Result<RotationState, String> {
    match fs::read_to_string(ROTATION_STATE_FILE) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|error| format!("rotation state is invalid: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RotationState::default()),
        Err(error) => Err(format!("could not read rotation state: {error}")),
    }
}

fn save_rotation_state(state: &RotationState) -> Result<(), String> {
    let state_path = StdPath::new(ROTATION_STATE_FILE);
    let state_directory = state_path
        .parent()
        .ok_or_else(|| "rotation state has no parent directory".to_string())?;
    fs::create_dir_all(state_directory)
        .map_err(|error| format!("could not create rotation state directory: {error}"))?;
    set_private_directory_mode(state_directory)?;

    let temporary_path = state_directory.join(format!(
        ".profile-rotation-state-{}.tmp",
        std::process::id()
    ));
    let serialized = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("could not serialize rotation state: {error}"))?;
    fs::write(&temporary_path, serialized)
        .map_err(|error| format!("could not write temporary rotation state: {error}"))?;
    set_private_file_mode(&temporary_path)?;
    fs::rename(&temporary_path, state_path)
        .map_err(|error| format!("could not atomically save rotation state: {error}"))?;
    Ok(())
}

fn write_profile_overlay(
    provider: Provider,
    profile: &ValidatedProfile,
) -> Result<PathBuf, String> {
    fs::create_dir_all(RUNTIME_DIR)
        .map_err(|error| format!("could not create runtime directory: {error}"))?;
    set_private_directory_mode(StdPath::new(RUNTIME_DIR))?;

    let contents = match provider {
        Provider::NordVpn => {
            let hostname = profile
                .endpoint_hostname
                .as_deref()
                .ok_or_else(|| "NordVPN profile did not yield a hostname".to_string())?;
            format!(
                "services:\n  vpn_gateway:\n    environment:\n      SERVER_COUNTRIES: \"\"\n      SERVER_HOSTNAMES: {}\n",
                yaml_double_quoted(hostname)
            )
        }
        Provider::ProtonVpn => format!(
            "services:\n  vpn_gateway:\n    environment:\n      SERVER_COUNTRIES: \"\"\n    volumes:\n      - {}\n",
            yaml_double_quoted(&format!(
                "{}:/gluetun/wireguard/wg0.conf:ro",
                profile.path.display()
            ))
        ),
        Provider::Direct => return Err("Direct mode has no VPN profile overlay".to_string()),
    };

    let path = PathBuf::from(PROFILE_OVERLAY_FILE);
    fs::write(&path, contents)
        .map_err(|error| format!("could not write runtime Compose overlay: {error}"))?;
    set_private_file_mode(&path)?;
    Ok(path)
}

fn yaml_double_quoted(value: &str) -> String {
    let escaped = value
        .chars()
        .flat_map(|character| match character {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            character => vec![character],
        })
        .collect::<String>();
    format!("\"{escaped}\"")
}

fn set_private_directory_mode(path: &StdPath) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect directory permissions: {error}"))
}

fn set_private_file_mode(path: &StdPath) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect file permissions: {error}"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/status", get(get_status))
        .route("/verify-leak", get(verify_leak))
        .route("/rotate/:provider", post(rotate_provider))
        .route("/rotate/:provider/profile", post(rotate_profile))
        .layer(CorsLayer::permissive())
        .with_state(AppState {
            rotation_lock: Arc::new(Mutex::new(())),
        });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Network Manager Daemon listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
