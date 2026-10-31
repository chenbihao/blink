//! Screenshot Interaction bridge: owned original pixels -> Capability -> ephemeral text.
use crate::app::command_error::CommandError;
use crate::domain::capability::{
    CapabilityRegistry, CapabilityResult, InvocationOrigin, InvokeContext, RuntimeCapabilities,
};
use crate::domain::context::qr_target::{QrTarget, classify};
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};
use tauri::Manager;

static SNAPSHOT_GATE: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
static TARGET_GATE: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
fn error(code: &str) -> CommandError {
    CommandError::new(code, "QR operation unavailable", true)
}
fn ensure_surface(window: &tauri::WebviewWindow) -> Result<(), CommandError> {
    if window.label() == "chord-screenshot" {
        Ok(())
    } else {
        Err(error("origin_denied"))
    }
}

#[tauri::command]
pub async fn screenshot_decode_qr(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    source: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<Value, CommandError> {
    use crate::domain::event::CapabilityEnv;
    use crate::domain::resource::{ResourceGrantSpec, ResourceUse, ReusePolicy};
    ensure_surface(&window)?;
    crate::infra::qr::validate_dimensions(w, h).map_err(|_| error("resource_limit"))?;
    let permit = SNAPSHOT_GATE
        .clone()
        .try_acquire_owned()
        .map_err(|_| error("busy"))?;
    // Freeze session identity before scheduling any CPU work. Arc-backed snapshot owns its pixels.
    let png = if source == "editor" {
        let bytes = crate::infra::platform::image_editor::session_png()
            .ok_or_else(|| error("invalid_state"))?;
        // The editor session PNG is already the original image, not the annotation canvas.
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (iw, ih) = crate::infra::platform::screenshot::parse_png_size(&bytes)
                .ok_or_else(|| error("invalid_image"))?;
            crate::infra::qr::validate_dimensions(iw, ih).map_err(|_| error("resource_limit"))?;
            let (rgba, iw, ih) = crate::infra::platform::screenshot::decode_png_to_rgba(&bytes)
                .map_err(|_| error("invalid_image"))?;
            let (mut crop, cw, ch) =
                crate::infra::platform::screenshot::crop_rgba(&rgba, iw, ih, x, y, w, h)
                    .ok_or_else(|| error("invalid_args"))?;
            for pixel in crop.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            crate::infra::platform::screenshot::encode_png(&crop, cw, ch)
                .map_err(|_| error("invalid_image"))
        });
        tokio::time::timeout(crate::infra::qr::DECODE_BUDGET, task)
            .await
            .map_err(|_| error("timeout"))?
            .map_err(|_| error("internal_error"))??
    } else if source == "screenshot" {
        let snapshot = crate::infra::platform::screenshot::owned_snapshot()
            .ok_or_else(|| error("invalid_state"))?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (pixels, iw, ih) = snapshot;
            let (crop, cw, ch) =
                crate::infra::platform::screenshot::crop_rgba(&pixels, iw, ih, x, y, w, h)
                    .ok_or_else(|| error("invalid_args"))?;
            crate::infra::platform::screenshot::encode_png(&crop, cw, ch)
                .map_err(|_| error("invalid_image"))
        });
        tokio::time::timeout(crate::infra::qr::DECODE_BUDGET, task)
            .await
            .map_err(|_| error("timeout"))?
            .map_err(|_| error("internal_error"))??
    } else {
        return Err(error("invalid_args"));
    };
    let env = app
        .state::<Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let store = env
        .resource_store()
        .ok_or_else(|| error("internal_error"))?;
    let image_ref = store
        .issue_memory(
            bytes::Bytes::from(png),
            "image/png",
            ResourceGrantSpec::new(
                ResourceUse::DecodeImage,
                ReusePolicy::Reusable,
                "screenshot.qr",
            ),
        )
        .map_err(|_| error("resource_limit"))?;
    struct Revoke<'a>(
        &'a crate::domain::resource::DefaultResourceStore,
        crate::domain::resource::ResourceRef,
    );
    impl Drop for Revoke<'_> {
        fn drop(&mut self) {
            self.0.revoke(&self.1);
        }
    }
    let _revoke = Revoke(store, image_ref.clone());
    let ctx = InvokeContext {
        env: env.as_ref(),
        origin: InvocationOrigin::LocalCommand,
        runtime: RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let registry = app.state::<Arc<CapabilityRegistry>>();
    let result = registry
        .invoke("decode_qr", json!({"image_ref":image_ref.as_str()}), &ctx)
        .await
        .map_err(CommandError::from)?;
    match result {
        CapabilityResult::Text { content, .. } => {
            serde_json::from_str(&content).map_err(|_| error("internal_error"))
        }
        _ => Err(error("internal_error")),
    }
}

/// Classify/open both revalidate the exact text. Client-provided action/target is never trusted.
#[tauri::command]
pub async fn screenshot_qr_target(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    text: String,
    execute: bool,
) -> Result<QrTarget, CommandError> {
    ensure_surface(&window)?;
    if text.len() > 64 * 1024 {
        return Err(error("resource_limit"));
    }
    let permit = TARGET_GATE
        .clone()
        .try_acquire_owned()
        .map_err(|_| error("busy"))?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        classify(&text)
    });
    let target = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .map_err(|_| error("timeout"))?
        .map_err(|_| error("internal_error"))?;
    if execute {
        let (id, args) = match &target {
            QrTarget::Web { target, .. } => ("open_url", json!({"url":target})),
            QrTarget::Directory { target } => ("open_path", json!({"path":target})),
            QrTarget::File { target } => ("reveal_in_explorer", json!({"path":target})),
            _ => return Err(error("invalid_args")),
        };
        let env = app
            .state::<Arc<crate::app::domain_env::TauriDomainEnv>>()
            .inner()
            .clone();
        let ctx = InvokeContext {
            env: env.as_ref(),
            origin: InvocationOrigin::LocalCommand,
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };
        app.state::<Arc<CapabilityRegistry>>()
            .invoke(id, args, &ctx)
            .await
            .map_err(|_| error("open_failed"))?;
        tracing::info!(action = id, "QR target action accepted by system");
    }
    Ok(target)
}
