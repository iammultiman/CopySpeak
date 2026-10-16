use crate::config::{self, sync_active_backend_mirror, AppConfig};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn set_active_profile(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    id: String,
) -> Result<(), String> {
    let mut cfg = config.lock().map_err(|e| e.to_string())?;
    if !cfg.tts.profiles.iter().any(|profile| profile.id == id) {
        return Err(format!("unknown profile: {}", id));
    }
    cfg.tts.active_profile_id = id;
    sync_active_backend_mirror(&mut cfg.tts);
    config::save(&cfg)?;
    // Warm the newly selected engine in the background. Without this, the
    // eager prewarm at app launch only ever covers whatever profile was active
    // then, so switching to a local engine (Kokoro, Piper) made the first
    // reading wait out the whole model load. Cloned and released before the
    // spawn so the config lock is not held across a multi-second model start.
    let tts = cfg.tts.clone();
    drop(cfg);
    std::thread::spawn(move || crate::commands::prewarm_active_profile(&tts));
    let _ = app.emit("config-changed", ());
    Ok(())
}
