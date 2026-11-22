//! The operations the tools need from Warren, as a trait so unit tests can
//! substitute an in-memory fake for the HTTP client in [`crate::sdk`].
//!
//! Methods return `impl Future + Send` so the server stays generic (no boxing,
//! no extra dependency) while the `#[tool]` futures remain `Send`.

use std::future::Future;

use serde::{Deserialize, Serialize};
use warren_core::{
    CreateInstanceRequest, ExecRequest, ExecResult, InputAction, Instance, InstanceStatus, Result,
    Template,
    agentapi::{DirListing, ResponseObject, ResponsesRequest},
};

/// `{ "id", "status" }` as answered by delete, stop, start and sleep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleAck {
    /// Instance id.
    pub id: String,
    /// The transitional status the instance entered.
    pub status: InstanceStatus,
}

/// `GET /v1/instances/{id}/signed-url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUrl {
    /// Browser-openable URL carrying a `warren_token`.
    pub url: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// `PUT /v1/files/content` on the Agent API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenFile {
    /// Path relative to the instance home.
    pub path: String,
    /// Bytes written.
    pub size: u64,
}

/// Everything the MCP tools call. One method per Warren operation; arguments
/// are already validated by the caller.
pub trait Backend: Send + Sync + 'static {
    /// `GET /v1/instances`.
    fn list_instances(&self) -> impl Future<Output = Result<Vec<Instance>>> + Send;
    /// `GET /v1/instances/{id}`.
    fn get_instance(&self, id: &str) -> impl Future<Output = Result<Instance>> + Send;
    /// `POST /v1/instances`; returns the instance in `provisioning`.
    fn create_instance(
        &self,
        req: CreateInstanceRequest,
    ) -> impl Future<Output = Result<Instance>> + Send;
    /// `DELETE /v1/instances/{id}`.
    fn delete_instance(&self, id: &str) -> impl Future<Output = Result<LifecycleAck>> + Send;
    /// `POST /v1/instances/{id}/stop`.
    fn stop_instance(&self, id: &str) -> impl Future<Output = Result<LifecycleAck>> + Send;
    /// `POST /v1/instances/{id}/start`.
    fn start_instance(&self, id: &str) -> impl Future<Output = Result<LifecycleAck>> + Send;
    /// `POST /v1/instances/{id}/sleep`.
    fn sleep_instance(&self, id: &str) -> impl Future<Output = Result<LifecycleAck>> + Send;
    /// `POST /v1/instances/{id}/exec`.
    fn exec(&self, id: &str, req: ExecRequest) -> impl Future<Output = Result<ExecResult>> + Send;
    /// `GET /v1/files/content?path=` on the instance: raw bytes.
    fn read_file(&self, id: &str, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;
    /// `PUT /v1/files/content?path=` on the instance.
    fn write_file(
        &self,
        id: &str,
        path: &str,
        content: Vec<u8>,
    ) -> impl Future<Output = Result<WrittenFile>> + Send;
    /// `GET /v1/files?path=` on the instance.
    fn list_files(&self, id: &str, path: &str) -> impl Future<Output = Result<DirListing>> + Send;
    /// `POST /v1/responses` on the instance, non-streaming.
    fn chat(
        &self,
        id: &str,
        req: ResponsesRequest,
    ) -> impl Future<Output = Result<ResponseObject>> + Send;
    /// `GET /v1/instances/{id}/signed-url?port=&ttl=`; `None` keeps the server defaults.
    fn signed_url(
        &self,
        id: &str,
        port: Option<u16>,
        ttl: Option<u32>,
    ) -> impl Future<Output = Result<SignedUrl>> + Send;
    /// `GET /v1/templates`.
    fn list_templates(&self) -> impl Future<Output = Result<Vec<Template>>> + Send;
    /// `GET /v1/instances/{id}/screenshot`: PNG bytes (mac instances only).
    fn screenshot(&self, id: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;
    /// `POST /v1/instances/{id}/input`: screen actions, in order.
    fn input(&self, id: &str, actions: Vec<InputAction>)
    -> impl Future<Output = Result<()>> + Send;
}
