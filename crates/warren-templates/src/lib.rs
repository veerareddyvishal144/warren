//! Template catalog, workspace templates and pins.
//!
//! [`Templates`] implements [`warren_core::TemplateService`]:
//!
//! - `ensure_system_templates` seeds the built-in catalog from
//!   [`warren_core::Config::system_templates`] at startup, preserving `created_at`
//!   and only publishing a new revision when a template's definition changed;
//! - `resolve` understands `name` and `name@revision` (a pin on a built-in
//!   template must equal its current revision);
//! - `list` returns the built-in catalog first, then the workspace's own
//!   templates, each group by name;
//! - `register` validates a [`warren_core::RegisterTemplateRequest`], pulls the
//!   image through the runtime registered for [`warren_core::InstanceType::Default`]
//!   and stores the template (the store assigns the revision);
//! - `delete` refuses built-in templates and templates still used by an instance.
//!
//! Persistence is delegated to the [`warren_core::Store`]; this crate holds no
//! state of its own.

mod service;
mod validate;

#[cfg(test)]
mod fakes;
#[cfg(test)]
mod tests;

pub use service::Templates;
pub use validate::{DEFAULT_MOUNT_PATH, RESERVED_NAME_PREFIX};
