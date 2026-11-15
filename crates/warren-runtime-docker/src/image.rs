//! Image presence and pulls.

use bollard::{models::ImageInspect, query_parameters::CreateImageOptions};
use futures::StreamExt;
use warren_core::{Error, Result};

use crate::{DockerRuntime, error};

/// Add `:latest` when a reference carries neither a tag nor a digest, so a pull
/// fetches one image instead of every tag of the repository.
pub(crate) fn normalize_image_ref(image_ref: &str) -> String {
    let r = image_ref.trim();
    if r.contains('@') {
        return r.to_string();
    }
    let name_start = r.rfind('/').map(|i| i + 1).unwrap_or(0);
    if r[name_start..].contains(':') {
        r.to_string()
    } else {
        format!("{r}:latest")
    }
}

/// The first repo digest, else the content id.
pub(crate) fn digest_of(img: &ImageInspect) -> Option<String> {
    img.repo_digests
        .as_ref()
        .and_then(|digests| digests.iter().find(|d| !d.is_empty()).cloned())
        .or_else(|| img.id.clone())
        .filter(|d| !d.is_empty())
}

/// A failed pull is the caller's problem when the daemon says the image does not
/// exist or may not be read; anything else is ours.
fn pull_failed(image: &str, message: &str) -> Error {
    let lower = message.to_ascii_lowercase();
    let user_fixable = [
        "not found",
        "manifest unknown",
        "does not exist",
        "denied",
        "unauthorized",
        "invalid reference",
    ];
    if user_fixable.iter().any(|needle| lower.contains(needle)) {
        Error::invalid(format!("image {image} could not be pulled: {message}"))
    } else {
        Error::internal(format!("pull {image}: {message}"))
    }
}

impl DockerRuntime {
    /// Make `image_ref` available locally and return its digest.
    pub(crate) async fn ensure_image_impl(&self, image_ref: &str) -> Result<String> {
        if image_ref.trim().is_empty() {
            return Err(Error::invalid("image reference must not be empty"));
        }
        let image = normalize_image_ref(image_ref);
        match self.docker.inspect_image(&image).await {
            Ok(img) => {
                let digest = digest_of(&img)
                    .ok_or_else(|| Error::internal(format!("image {image} has no digest or id")))?;
                tracing::debug!(%image, %digest, "image present");
                return Ok(digest);
            }
            Err(e) if error::is_not_found(&e) => {}
            Err(e) => return Err(error::map(e, "image")),
        }
        tracing::info!(%image, "pulling image");
        let options = CreateImageOptions {
            from_image: Some(image.clone()),
            ..Default::default()
        };
        let mut stream = std::pin::pin!(self.docker.create_image(Some(options), None, None));
        while let Some(item) = stream.next().await {
            match item {
                Ok(progress) => {
                    if let Some(detail) = progress.error_detail {
                        return Err(pull_failed(&image, &detail.message.unwrap_or_default()));
                    }
                    tracing::debug!(
                        %image,
                        status = progress.status.as_deref().unwrap_or(""),
                        layer = progress.id.as_deref().unwrap_or(""),
                        "pull progress"
                    );
                }
                Err(e) => return Err(pull_failed(&image, &error::message(&e))),
            }
        }
        let img = self
            .docker
            .inspect_image(&image)
            .await
            .map_err(|e| error::map(e, "image"))?;
        let digest = digest_of(&img)
            .ok_or_else(|| Error::internal(format!("image {image} has no digest or id")))?;
        tracing::info!(%image, %digest, "image pulled");
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_references() {
        assert_eq!(normalize_image_ref("alpine"), "alpine:latest");
        assert_eq!(normalize_image_ref(" alpine:3.20 "), "alpine:3.20");
        assert_eq!(normalize_image_ref("warren/base:dev"), "warren/base:dev");
        assert_eq!(
            normalize_image_ref("registry.local:5000/team/app"),
            "registry.local:5000/team/app:latest"
        );
        assert_eq!(
            normalize_image_ref("registry.local:5000/team/app:v1"),
            "registry.local:5000/team/app:v1"
        );
        assert_eq!(
            normalize_image_ref("alpine@sha256:abc"),
            "alpine@sha256:abc"
        );
    }

    #[test]
    fn digests() {
        let img = ImageInspect {
            id: Some("sha256:id".into()),
            repo_digests: Some(vec!["alpine@sha256:repo".into()]),
            ..Default::default()
        };
        assert_eq!(digest_of(&img).as_deref(), Some("alpine@sha256:repo"));
        let local = ImageInspect {
            id: Some("sha256:id".into()),
            repo_digests: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(digest_of(&local).as_deref(), Some("sha256:id"));
        assert_eq!(digest_of(&ImageInspect::default()), None);
    }

    #[test]
    fn classifies_pull_failures() {
        assert_eq!(
            pull_failed("x", "manifest unknown").code,
            warren_core::errors::CODE_INVALID_REQUEST
        );
        assert_eq!(
            pull_failed("x", "pull access denied for x").code,
            warren_core::errors::CODE_INVALID_REQUEST
        );
        assert_eq!(
            pull_failed("x", "connection reset").code,
            warren_core::errors::CODE_INTERNAL
        );
    }
}
