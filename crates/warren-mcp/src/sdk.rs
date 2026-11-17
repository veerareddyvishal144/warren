//! Adapter from [`warren_sdk::Client`] to [`Backend`]. This is the only module
//! that names SDK methods; everything else is tested against a fake.

use std::{collections::HashMap, sync::Mutex};

use warren_core::{
    CreateInstanceRequest, Error, ExecRequest, ExecResult, Instance, Result, Template,
    agentapi::{DirListing, ResponseObject, ResponsesRequest},
};
use warren_sdk::AgentClient;

use crate::{
    Settings,
    backend::{Backend, LifecycleAck, SignedUrl, WrittenFile},
};

/// Warren reached through the Rust SDK.
pub struct SdkBackend {
    client: warren_sdk::Client,
    /// Instance URL by id. The Agent API lives at the instance URL, which the
    /// Sandbox API derives from the id and the base domain, so it never changes
    /// and one lookup per instance is enough.
    urls: Mutex<HashMap<String, String>>,
}

impl SdkBackend {
    /// Wrap an existing client.
    pub fn new(client: warren_sdk::Client) -> Self {
        SdkBackend {
            client,
            urls: Mutex::new(HashMap::new()),
        }
    }

    /// Build a client from the process settings.
    pub fn connect(settings: &Settings) -> Self {
        Self::new(warren_sdk::Client::new(
            &settings.api_url,
            &settings.api_key,
        ))
    }

    /// The underlying Sandbox API client.
    pub fn client(&self) -> &warren_sdk::Client {
        &self.client
    }

    fn cached_url(&self, id: &str) -> Option<String> {
        self.urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// An Agent API client for the instance, resolving its URL on first use.
    async fn agent(&self, id: &str) -> Result<AgentClient> {
        let url = match self.cached_url(id) {
            Some(url) => url,
            None => {
                let inst = self.client.instance(id).await?;
                if inst.url.is_empty() {
                    return Err(Error::internal(format!(
                        "instance {id} has no URL; the API did not derive one"
                    )));
                }
                self.urls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.to_string(), inst.url.clone());
                inst.url
            }
        };
        Ok(self.client.agent(url))
    }
}

impl Backend for SdkBackend {
    async fn list_instances(&self) -> Result<Vec<Instance>> {
        Ok(self.client.instances().await?)
    }

    async fn get_instance(&self, id: &str) -> Result<Instance> {
        Ok(self.client.instance(id).await?)
    }

    async fn create_instance(&self, req: CreateInstanceRequest) -> Result<Instance> {
        Ok(self.client.create_instance(&req).await?)
    }

    async fn delete_instance(&self, id: &str) -> Result<LifecycleAck> {
        let r = self.client.delete_instance(id).await?;
        self.urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(LifecycleAck {
            id: r.id,
            status: r.status,
        })
    }

    async fn stop_instance(&self, id: &str) -> Result<LifecycleAck> {
        let r = self.client.stop(id).await?;
        Ok(LifecycleAck {
            id: r.id,
            status: r.status,
        })
    }

    async fn start_instance(&self, id: &str) -> Result<LifecycleAck> {
        let r = self.client.start(id).await?;
        Ok(LifecycleAck {
            id: r.id,
            status: r.status,
        })
    }

    async fn sleep_instance(&self, id: &str) -> Result<LifecycleAck> {
        let r = self.client.sleep(id).await?;
        Ok(LifecycleAck {
            id: r.id,
            status: r.status,
        })
    }

    async fn exec(&self, id: &str, req: ExecRequest) -> Result<ExecResult> {
        Ok(self.client.exec(id, &req).await?)
    }

    async fn read_file(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        Ok(self.agent(id).await?.read_file(path).await?)
    }

    async fn write_file(&self, id: &str, path: &str, content: Vec<u8>) -> Result<WrittenFile> {
        let r = self.agent(id).await?.write_file(path, content).await?;
        Ok(WrittenFile {
            path: r.path,
            size: r.size,
        })
    }

    async fn list_files(&self, id: &str, path: &str) -> Result<DirListing> {
        Ok(self.agent(id).await?.list_files(path).await?)
    }

    async fn chat(&self, id: &str, req: ResponsesRequest) -> Result<ResponseObject> {
        Ok(self.agent(id).await?.respond(&req).await?)
    }

    async fn signed_url(&self, id: &str, port: Option<u16>, ttl: Option<u32>) -> Result<SignedUrl> {
        let r = self
            .client
            .signed_url(id, port.unwrap_or(0), i64::from(ttl.unwrap_or(0)), "")
            .await?;
        Ok(SignedUrl {
            url: r.url,
            expires_at: r.expires_at,
        })
    }

    async fn list_templates(&self) -> Result<Vec<Template>> {
        Ok(self.client.templates().await?)
    }
}
