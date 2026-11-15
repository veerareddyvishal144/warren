//! The [`Templates`] service: catalog seeding, name and pin resolution,
//! registration with image pulls, and deletion.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use warren_core::{
    Clock, Config, Error, InstanceType, RealClock, RegisterTemplateRequest, Result, Runtime,
    Runtimes, Shape, Store, Template, TemplateScope, TemplateService,
};

use crate::validate::{self, TemplateRef};

/// Instance types a registered workspace template may run as.
const WORKSPACE_TEMPLATE_TYPES: [InstanceType; 2] = [InstanceType::Default, InstanceType::Vm];

/// Shape instances of a registered workspace template start with.
const WORKSPACE_TEMPLATE_SHAPE: Shape = Shape {
    cpu: 2,
    memory: 4,
    disk: 4,
};

/// Template catalog backed by a [`Store`]. Images are pulled through the runtime
/// registered for [`InstanceType::Default`].
pub struct Templates {
    store: Arc<dyn Store>,
    runtimes: Runtimes,
    cfg: Config,
    clock: Arc<dyn Clock>,
}

impl Templates {
    /// Build the service on the real clock.
    pub fn new(store: Arc<dyn Store>, runtimes: Runtimes, cfg: Config) -> Arc<Templates> {
        Self::with_clock(store, runtimes, cfg, Arc::new(RealClock))
    }

    /// Build the service with an explicit time source; tests pass a fixed clock.
    pub fn with_clock(
        store: Arc<dyn Store>,
        runtimes: Runtimes,
        cfg: Config,
        clock: Arc<dyn Clock>,
    ) -> Arc<Templates> {
        Arc::new(Templates {
            store,
            runtimes,
            cfg,
            clock,
        })
    }

    /// The runtime that pulls images: the default-type one, which every
    /// deployment has.
    fn image_runtime(&self) -> Result<&Arc<dyn Runtime>> {
        self.runtimes
            .get(&InstanceType::Default)
            .ok_or_else(|| Error::not_supported("no runtime is available to pull template images"))
    }

    /// Look up a template, mapping "absent" to `None` and passing other failures up.
    async fn find(&self, workspace_id: &str, name: &str) -> Result<Option<Template>> {
        match self.store.get_template(workspace_id, name).await {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Fetch a parsed reference: the current template for a bare name, a stored
    /// revision for a pinned workspace template, and the current revision for a
    /// pinned system template when the pin matches it.
    async fn fetch(&self, workspace_id: &str, r: TemplateRef<'_>) -> Result<Template> {
        let current = self.store.get_template(workspace_id, r.name).await?;
        let Some(revision) = r.revision else {
            return Ok(current);
        };
        match current.scope {
            TemplateScope::Workspace => {
                self.store
                    .get_template_revision(workspace_id, r.name, revision)
                    .await
            }
            TemplateScope::System if current.revision == revision => Ok(current),
            TemplateScope::System => Err(Error::not_found("template revision")),
        }
    }

    /// Seed or refresh one built-in template. Returns the revision now stored.
    async fn ensure_system_template(&self, mut wanted: Template, now: i64) -> Result<u32> {
        wanted.scope = TemplateScope::System;
        wanted.workspace_id.clear();
        wanted.updated_at = now;
        let Some(current) = self.find("", &wanted.name).await? else {
            wanted.revision = wanted.revision.max(1);
            wanted.created_at = now;
            self.store.upsert_template(&wanted).await?;
            tracing::info!(template = %wanted.name, image = %wanted.image_ref, "seeded system template");
            return Ok(wanted.revision);
        };
        if same_definition(&current, &wanted) {
            tracing::debug!(template = %wanted.name, revision = current.revision, "system template unchanged");
            return Ok(current.revision);
        }
        wanted.revision = current.revision.saturating_add(1);
        wanted.created_at = current.created_at;
        if wanted.image_ref == current.image_ref {
            // Same image, other fields changed: the recorded digest still holds.
            wanted.image_digest = current.image_digest.clone();
        }
        self.store.upsert_template(&wanted).await?;
        tracing::info!(
            template = %wanted.name,
            revision = wanted.revision,
            image = %wanted.image_ref,
            "published a new revision of a system template"
        );
        Ok(wanted.revision)
    }
}

/// The fields that define a revision of a built-in template. The description and
/// the timestamps may change without publishing a new revision; `types` compares
/// as a set.
fn same_definition(a: &Template, b: &Template) -> bool {
    let types = |t: &Template| t.types.iter().map(|k| k.as_str()).collect::<BTreeSet<_>>();
    a.image_ref == b.image_ref
        && a.default_port == b.default_port
        && a.mount_path == b.mount_path
        && a.agent == b.agent
        && a.env == b.env
        && types(a) == types(b)
        && a.default_shape == b.default_shape
}

#[async_trait]
impl TemplateService for Templates {
    async fn resolve(&self, workspace_id: &str, name: &str) -> Result<Template> {
        self.fetch(workspace_id, validate::parse_ref(name)?).await
    }

    /// Delegates to the store. A `name@revision` reference is accepted as well
    /// and behaves like [`TemplateService::resolve`], since no stored name can
    /// contain `@`.
    async fn get(&self, workspace_id: &str, name: &str) -> Result<Template> {
        self.fetch(workspace_id, validate::parse_ref(name)?).await
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<Template>> {
        let mut all = self.store.list_templates(workspace_id).await?;
        let rank = |t: &Template| u8::from(t.scope != TemplateScope::System);
        all.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name)));
        Ok(all)
    }

    async fn register(
        &self,
        workspace_id: &str,
        req: &RegisterTemplateRequest,
    ) -> Result<Template> {
        if workspace_id.is_empty() {
            return Err(Error::invalid(
                "a workspace is required to register a template",
            ));
        }
        validate::validate_name(&req.name)?;
        let fields = validate::validate_register(req)?;
        let existing = match self.find(workspace_id, &req.name).await? {
            Some(t) if t.scope == TemplateScope::System => {
                return Err(Error::conflict(format!(
                    "{:?} is a built-in template",
                    req.name
                )));
            }
            Some(t) if t.workspace_id != workspace_id => None,
            other => other,
        };
        let runtime = self.image_runtime()?;
        let image_digest = runtime.ensure_image(&fields.image_ref).await?;
        let now = self.clock.now_unix();
        let (revision, created_at) = match &existing {
            Some(t) => (t.revision.saturating_add(1), t.created_at),
            None => (1, now),
        };
        let template = Template {
            name: req.name.clone(),
            scope: TemplateScope::Workspace,
            workspace_id: workspace_id.to_string(),
            image_ref: fields.image_ref,
            image_digest,
            default_port: req.default_port,
            mount_path: fields.mount_path,
            agent: req.agent,
            description: fields.description,
            types: WORKSPACE_TEMPLATE_TYPES.to_vec(),
            default_shape: WORKSPACE_TEMPLATE_SHAPE,
            env: req.env.clone(),
            revision,
            created_at,
            updated_at: now,
        };
        self.store.upsert_template(&template).await?;
        let stored = self.store.get_template(workspace_id, &req.name).await?;
        tracing::info!(
            workspace = %workspace_id,
            template = %stored.name,
            revision = stored.revision,
            image = %stored.image_ref,
            digest = %stored.image_digest,
            "registered template"
        );
        Ok(stored)
    }

    async fn delete(&self, workspace_id: &str, name: &str) -> Result<()> {
        if workspace_id.is_empty() {
            return Err(Error::invalid(
                "a workspace is required to delete a template",
            ));
        }
        let t = self.store.get_template(workspace_id, name).await?;
        if t.scope == TemplateScope::System {
            return Err(Error::forbidden("built-in templates cannot be deleted"));
        }
        if t.workspace_id != workspace_id {
            return Err(Error::not_found("template"));
        }
        let in_use = self
            .store
            .count_instances_using_template(workspace_id, name)
            .await?;
        if in_use > 0 {
            return Err(Error::conflict(format!(
                "{in_use} instance(s) still use template {name:?}; delete them first"
            )));
        }
        self.store.delete_template(workspace_id, name).await?;
        tracing::info!(workspace = %workspace_id, template = %name, "deleted template");
        Ok(())
    }

    async fn ensure_system_templates(&self) -> Result<()> {
        let now = self.clock.now_unix();
        let catalog = self.cfg.system_templates();
        tracing::debug!(count = catalog.len(), image_tag = %self.cfg.image_tag, "ensuring system templates");
        for wanted in catalog {
            self.ensure_system_template(wanted, now).await?;
        }
        Ok(())
    }
}
