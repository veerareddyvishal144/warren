//! Gateway injection for images that do not ship `warren-gateway`.
//!
//! The built-in (system) images contain the gateway and run it as their
//! entrypoint. A workspace template registered from an arbitrary image
//! (`alpine:3.20`, an n8n image) has no gateway and often no long-running
//! process, so left alone its container exits before it gets an IP and the
//! instance fails. When the daemon knows a fully static gateway binary
//! (`Config::gateway_bin`) the runtime bind-mounts it read-only at
//! [`Config::INJECTED_GATEWAY_PATH`] and makes it the container's entrypoint in
//! wrap mode:
//!
//! ```text
//! /warren/warren-gateway --serve --wrap -- <image Entrypoint...> <image Cmd...>
//! ```
//!
//! The gateway serves the Warren API on the gateway port and supervises the
//! image's own entrypoint and command as a child. An image with neither gets
//! `/warren/warren-gateway --serve`. The image's `User` and `WorkingDir` are
//! kept, so nothing runs as root that did not already; env, labels, mounts,
//! network and resource settings are untouched, and the container carries the
//! label [`LABEL_INJECTED`]`=true`.
//!
//! The decision is by template scope: workspace templates are injected, system
//! templates already contain the gateway. A workspace template without a
//! configured gateway binary is created unchanged with a warning; when `start`
//! then finds nothing listening, [`explain_missing_gateway`] appends a hint that
//! names `--gateway-bin` as the fix.

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use bollard::models::{ContainerInspectResponse, ImageInspect};
use warren_core::{Config, Error, Result, Template, TemplateScope};

use crate::{DockerRuntime, LABEL_INJECTED, LABEL_SCOPE, error, image::normalize_image_ref};

/// Arguments that put the injected gateway into wrap mode; the image's own
/// entrypoint and command follow them.
pub(crate) const WRAP_ARGS: [&str; 3] = ["--serve", "--wrap", "--"];
/// The only argument when the image has nothing to wrap.
pub(crate) const SERVE_ARG: &str = "--serve";
/// Appended to a start failure of a workspace-template container that was
/// created without an injected gateway.
pub(crate) const MISSING_GATEWAY_HINT: &str = "the template image does not ship the Warren gateway and no gateway binary was configured when the container was created; start warrend with --gateway-bin <path to the static warren-gateway> and recreate the instance";

/// The parts of an image configuration that injection needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImageCommand {
    /// `Config.Entrypoint`; empty when the image has none.
    pub(crate) entrypoint: Vec<String>,
    /// `Config.Cmd`; empty when the image has none.
    pub(crate) cmd: Vec<String>,
    /// `Config.User`; empty means the image default (root).
    pub(crate) user: String,
    /// `Config.WorkingDir`; empty means the image default.
    pub(crate) working_dir: String,
}

impl ImageCommand {
    /// Read the entrypoint, command, user and working directory of an inspected image.
    pub(crate) fn from_inspect(img: &ImageInspect) -> ImageCommand {
        let cfg = img.config.as_ref();
        let list = |v: Option<&Vec<String>>| v.cloned().unwrap_or_default();
        let text = |v: Option<&String>| v.map(|s| s.trim().to_string()).unwrap_or_default();
        ImageCommand {
            entrypoint: list(cfg.and_then(|c| c.entrypoint.as_ref())),
            cmd: list(cfg.and_then(|c| c.cmd.as_ref())),
            user: text(cfg.and_then(|c| c.user.as_ref())),
            working_dir: text(cfg.and_then(|c| c.working_dir.as_ref())),
        }
    }

    /// Whether the image runs as root (no `User`, `root`, or uid 0).
    pub(crate) fn runs_as_root(&self) -> bool {
        let user = self.user.split(':').next().unwrap_or("").trim();
        user.is_empty() || user == "root" || user == "0"
    }
}

/// What the create-container body needs to inject the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Injection {
    /// Absolute host path of the static gateway binary.
    pub(crate) gateway_bin: PathBuf,
    /// The image's own entrypoint, command, user and working directory.
    pub(crate) image: ImageCommand,
}

/// The gateway binary injected into containers of `tpl`, if any: only workspace
/// templates get one (system images ship the gateway) and only when the daemon
/// knows a binary.
pub(crate) fn gateway_for_template<'a>(
    tpl: &Template,
    gateway_bin: Option<&'a Path>,
) -> Option<&'a Path> {
    match tpl.scope {
        TemplateScope::System => None,
        TemplateScope::Workspace => gateway_bin,
    }
}

/// The container entrypoint of an injected image: the gateway in wrap mode
/// followed by the image's entrypoint and command, or just `--serve` when the
/// image has neither.
pub(crate) fn wrap_entrypoint(entrypoint: &[String], cmd: &[String]) -> Vec<String> {
    let mut argv = vec![Config::INJECTED_GATEWAY_PATH.to_string()];
    if entrypoint.is_empty() && cmd.is_empty() {
        argv.push(SERVE_ARG.to_string());
    } else {
        argv.extend(WRAP_ARGS.iter().map(|a| a.to_string()));
        argv.extend(entrypoint.iter().cloned());
        argv.extend(cmd.iter().cloned());
    }
    argv
}

/// The Docker bind spec for the gateway: `<host path>:/warren/warren-gateway:ro`.
pub(crate) fn gateway_bind(gateway_bin: &Path) -> String {
    format!(
        "{}:{}:ro",
        gateway_bin.display(),
        Config::INJECTED_GATEWAY_PATH
    )
}

/// Whether a container's labels say it came from a workspace template and has no
/// injected gateway: nothing inside it is expected to serve the Warren API.
pub(crate) fn lacks_gateway(labels: Option<&HashMap<String, String>>) -> bool {
    let Some(labels) = labels else { return false };
    labels.get(LABEL_SCOPE).map(String::as_str) == Some(TemplateScope::Workspace.as_str())
        && labels.get(LABEL_INJECTED).map(String::as_str) != Some("true")
}

/// Add [`MISSING_GATEWAY_HINT`] to a start failure when the container
/// [`lacks_gateway`]; other errors pass through unchanged.
pub(crate) fn explain_missing_gateway(mut err: Error, inspect: &ContainerInspectResponse) -> Error {
    let labels = inspect.config.as_ref().and_then(|c| c.labels.as_ref());
    if lacks_gateway(labels) {
        err.message = format!("{}; {MISSING_GATEWAY_HINT}", err.message);
    }
    err
}

/// Resolve the configured gateway binary to an absolute path and make sure it is
/// a file. Docker would turn a missing bind source into a new directory and a
/// relative one into a named volume, so neither may reach the create request.
pub(crate) async fn check_gateway_bin(path: &Path) -> Result<PathBuf> {
    let problem = |what: String| {
        Error::internal(format!(
            "gateway binary {}: {what}; fix --gateway-bin",
            path.display()
        ))
    };
    let resolved = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| problem(format!("cannot resolve: {e}")))?;
    let meta = tokio::fs::metadata(&resolved)
        .await
        .map_err(|e| problem(format!("cannot stat: {e}")))?;
    if !meta.is_file() {
        return Err(problem("not a regular file".to_string()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(problem("not executable".to_string()));
    }
    Ok(resolved)
}

impl DockerRuntime {
    /// Decide and prepare the injection for a container of `tpl` running `image`.
    /// `None` means the container is created unchanged: always for system
    /// templates, and with a warning for workspace templates when no gateway
    /// binary is configured.
    pub(crate) async fn injection_for(
        &self,
        tpl: &Template,
        image: &str,
    ) -> Result<Option<Injection>> {
        let Some(bin) = gateway_for_template(tpl, self.cfg.gateway_bin.as_deref()) else {
            if tpl.scope == TemplateScope::Workspace {
                tracing::warn!(
                    template = %tpl.name,
                    %image,
                    "no gateway binary configured: the image is started unchanged and must ship its own gateway or the instance will not start (set --gateway-bin)"
                );
            }
            return Ok(None);
        };
        let gateway_bin = check_gateway_bin(bin).await?;
        let inspected = self
            .docker
            .inspect_image(&normalize_image_ref(image))
            .await
            .map_err(|e| error::map(e, "image"))?;
        let command = ImageCommand::from_inspect(&inspected);
        if !command.runs_as_root() {
            let mode = tokio::fs::metadata(&gateway_bin)
                .await
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
            if mode & 0o005 != 0o005 {
                tracing::warn!(
                    gateway_bin = %gateway_bin.display(),
                    mode = format!("{:o}", mode & 0o777),
                    user = %command.user,
                    "the gateway binary is not world-readable and executable; the image's non-root user may fail to run it"
                );
            }
        }
        tracing::debug!(
            template = %tpl.name,
            %image,
            gateway_bin = %gateway_bin.display(),
            entrypoint = ?command.entrypoint,
            cmd = ?command.cmd,
            user = %command.user,
            working_dir = %command.working_dir,
            "injecting the gateway"
        );
        Ok(Some(Injection {
            gateway_bin,
            image: command,
        }))
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{ContainerConfig, ImageConfig};

    use super::*;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    const GW: &str = "/warren/warren-gateway";

    #[test]
    fn wraps_entrypoint_only() {
        assert_eq!(
            wrap_entrypoint(&strings(&["/docker-entrypoint.sh"]), &[]),
            strings(&[GW, "--serve", "--wrap", "--", "/docker-entrypoint.sh"])
        );
    }

    #[test]
    fn wraps_cmd_only() {
        assert_eq!(
            wrap_entrypoint(&[], &strings(&["/bin/sh"])),
            strings(&[GW, "--serve", "--wrap", "--", "/bin/sh"])
        );
    }

    #[test]
    fn wraps_entrypoint_then_cmd() {
        assert_eq!(
            wrap_entrypoint(
                &strings(&["/docker-entrypoint.sh"]),
                &strings(&["nginx", "-g", "daemon off;"])
            ),
            strings(&[
                GW,
                "--serve",
                "--wrap",
                "--",
                "/docker-entrypoint.sh",
                "nginx",
                "-g",
                "daemon off;"
            ])
        );
    }

    #[test]
    fn serves_alone_when_the_image_has_neither() {
        assert_eq!(wrap_entrypoint(&[], &[]), strings(&[GW, "--serve"]));
    }

    #[test]
    fn bind_is_read_only_at_the_injected_path() {
        assert_eq!(
            gateway_bind(Path::new("/opt/warren/warren-gateway")),
            "/opt/warren/warren-gateway:/warren/warren-gateway:ro"
        );
        assert_eq!(Config::INJECTED_GATEWAY_PATH, GW);
    }

    #[test]
    fn only_workspace_templates_with_a_binary_are_injected() {
        let bin = Path::new("/usr/local/bin/warren-gateway");
        let workspace = Template {
            scope: TemplateScope::Workspace,
            ..Template::default()
        };
        let system = Template {
            scope: TemplateScope::System,
            ..Template::default()
        };
        assert_eq!(gateway_for_template(&workspace, Some(bin)), Some(bin));
        assert_eq!(gateway_for_template(&workspace, None), None);
        assert_eq!(gateway_for_template(&system, Some(bin)), None);
        assert_eq!(gateway_for_template(&system, None), None);
    }

    #[test]
    fn reads_the_image_command() {
        let img = ImageInspect {
            config: Some(ImageConfig {
                entrypoint: Some(strings(&["docker-entrypoint.sh"])),
                cmd: Some(strings(&["redis-server"])),
                user: Some(" redis ".into()),
                working_dir: Some("/data".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let command = ImageCommand::from_inspect(&img);
        assert_eq!(
            command,
            ImageCommand {
                entrypoint: strings(&["docker-entrypoint.sh"]),
                cmd: strings(&["redis-server"]),
                user: "redis".into(),
                working_dir: "/data".into(),
            }
        );
        assert!(!command.runs_as_root());

        let bare = ImageCommand::from_inspect(&ImageInspect::default());
        assert_eq!(bare, ImageCommand::default());
        assert!(bare.runs_as_root());
        let alpine = ImageInspect {
            config: Some(ImageConfig {
                cmd: Some(strings(&["/bin/sh"])),
                user: Some(String::new()),
                working_dir: Some("/".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let alpine = ImageCommand::from_inspect(&alpine);
        assert!(alpine.entrypoint.is_empty() && alpine.cmd == strings(&["/bin/sh"]));
        assert_eq!(alpine.working_dir, "/");
        for user in ["", "root", "0", "0:0", "root:root"] {
            let c = ImageCommand {
                user: user.into(),
                ..Default::default()
            };
            assert!(c.runs_as_root(), "{user:?}");
        }
        for user in ["node", "1000", "1000:1000", "node:node"] {
            let c = ImageCommand {
                user: user.into(),
                ..Default::default()
            };
            assert!(!c.runs_as_root(), "{user:?}");
        }
    }

    fn labelled(pairs: &[(&str, &str)]) -> ContainerInspectResponse {
        ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(
                    pairs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn detects_workspace_containers_without_a_gateway() {
        let labels = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert!(lacks_gateway(Some(&labels(&[(LABEL_SCOPE, "workspace")]))));
        assert!(!lacks_gateway(Some(&labels(&[
            (LABEL_SCOPE, "workspace"),
            (LABEL_INJECTED, "true")
        ]))));
        assert!(!lacks_gateway(Some(&labels(&[(LABEL_SCOPE, "system")]))));
        assert!(
            !lacks_gateway(Some(&labels(&[]))),
            "containers from before the label existed are not accused"
        );
        assert!(!lacks_gateway(None));
    }

    #[test]
    fn start_failures_name_gateway_bin_only_when_it_is_the_cause() {
        let err = Error::bad_gateway("container exited with code 0 before listening on port 3737");
        let explained = explain_missing_gateway(
            err.clone(),
            &labelled(&[
                (LABEL_SCOPE, "workspace"),
                ("warren.instance", "ab12cd34ef"),
            ]),
        );
        assert_eq!(explained.code, err.code);
        assert!(explained.message.starts_with(&err.message));
        assert!(
            explained.message.contains("--gateway-bin"),
            "{}",
            explained.message
        );
        let untouched = explain_missing_gateway(
            err.clone(),
            &labelled(&[(LABEL_SCOPE, "workspace"), (LABEL_INJECTED, "true")]),
        );
        assert_eq!(untouched, err);
        let system = explain_missing_gateway(err.clone(), &labelled(&[(LABEL_SCOPE, "system")]));
        assert_eq!(system, err);
        assert_eq!(
            explain_missing_gateway(err.clone(), &ContainerInspectResponse::default()),
            err
        );
    }

    #[tokio::test]
    async fn gateway_bin_must_be_an_executable_file() {
        let dir = crate::tests::scratch_dir("gateway-bin");
        let missing = dir.join("missing");
        let err = check_gateway_bin(&missing).await.unwrap_err();
        assert!(err.is_internal());
        assert!(
            err.details
                .as_ref()
                .map(|d| d.to_string())
                .unwrap_or_default()
                .contains("--gateway-bin"),
            "{err:?}"
        );
        assert!(check_gateway_bin(&dir).await.is_err(), "a directory");
        let plain = dir.join("plain");
        std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_gateway_bin(&plain).await.is_err(), "not executable");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = check_gateway_bin(&plain).await.unwrap();
        assert!(resolved.is_absolute() && resolved.ends_with("plain"));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&plain, &link).unwrap();
        assert_eq!(check_gateway_bin(&link).await.unwrap(), resolved);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
