use crate::capability::*;
use crate::resource::ResourceUse;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

pub struct DecodeQr;
static DECODE_GATE: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

fn decode_error(error: blink_infra::qr::QrError) -> CapabilityError {
    use blink_infra::qr::QrError;
    let category = match error {
        QrError::Timeout => {
            return CapabilityError::Timeout {
                detail: "QR decoding timed out".into(),
            };
        }
        QrError::ResourceLimit => "resource_limit",
        QrError::InvalidImage => "invalid_image",
        QrError::DecoderFailed => "decoder_failed",
    };
    CapabilityError::Backend {
        category: category.into(),
        message: error.to_string(),
        detail: None,
        retryable: false,
    }
}

#[async_trait::async_trait]
impl Capability for DecodeQr {
    fn id(&self) -> &str {
        "decode_qr"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Decode QR text from an owned PNG image reference locally".into(),
            parameters: json!({"type":"object","properties":{"image_ref":{"type":"string"}},"required":["image_ref"],"additionalProperties":false}),
            sensitive: true,
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_COMMAND | OriginSet::LOCAL_SURFACE,
            runtime_requirement: RuntimeRequirement::NONE,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "QR request expired".into(),
            });
        }
        let permit =
            DECODE_GATE
                .clone()
                .try_acquire_owned()
                .map_err(|_| CapabilityError::Backend {
                    category: "busy".into(),
                    message: "QR decoder busy".into(),
                    detail: None,
                    retryable: true,
                })?;
        let png = super::image_input::resolve_image_ref(
            &args,
            ctx.env.resource_store().map(Arc::as_ref),
            ResourceUse::DecodeImage,
        )?;
        let budget = ctx
            .deadline
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(blink_infra::qr::DECODE_BUDGET)
            .min(blink_infra::qr::DECODE_BUDGET);
        let job = tokio::task::spawn_blocking(move || {
            // The worker retains admission even after the caller times out or drops.
            let _permit = permit;
            blink_infra::qr::decode_png(&png)
        });
        let texts = tokio::time::timeout(budget, job)
            .await
            .map_err(|_| CapabilityError::Timeout {
                detail: "QR decoding timed out".into(),
            })?
            .map_err(|_| CapabilityError::Internal {
                detail: "QR worker failed".into(),
            })?
            .map_err(decode_error)?;
        Ok(CapabilityResult::Text {
            content: serde_json::to_string(&texts).map_err(|_| CapabilityError::Internal {
                detail: "QR result encoding failed".into(),
            })?,
            desc: None,
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(DecodeQr) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_is_registered_and_does_not_grant_ai_or_external_access() {
        let registry = CapabilityRegistry::new();
        let cap = registry.get("decode_qr").unwrap();
        assert!(cap.policy().allows_origin(InvocationOrigin::LocalCommand));
        for origin in [
            InvocationOrigin::LocalAi,
            InvocationOrigin::Cli,
            InvocationOrigin::Mcp,
        ] {
            assert!(!cap.policy().allows_origin(origin));
        }
        assert!(
            matches!(decode_error(blink_infra::qr::QrError::ResourceLimit), CapabilityError::Backend { category, .. } if category == "resource_limit")
        );
        assert!(matches!(
            decode_error(blink_infra::qr::QrError::Timeout),
            CapabilityError::Timeout { .. }
        ));
    }
}
