//! Data model shared between the UI and backend worker.

#[derive(Debug, Clone)]
#[allow(dead_code)] // Spec fields; not all are used by the UI.
pub enum AccountSource {
    AzureSubscription {
        subscription_id: String,
        subscription_name: String,
        resource_group: String,
        tenant_id: Option<String>,
    },
    AttachedManual {
        profile_name: String,
    },
}

#[derive(Clone)]
pub enum AuthCredential {
    /// Token obtained from Azure CLI for ARM / data plane.
    BearerToken(String),
    /// Account key or SAS obtained from ARM listKeys or a Connection String.
    SharedKey {
        account_name: String,
        account_key: String,
    },
    SasToken(String),
}

impl AuthCredential {
    pub fn label(&self) -> &'static str {
        match self {
            AuthCredential::BearerToken(_) => "Entra ID (Bearer)",
            AuthCredential::SharedKey { .. } => "Shared Key",
            AuthCredential::SasToken(_) => "SAS",
        }
    }
}

impl std::fmt::Debug for AuthCredential {
    // Never print secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuthCredential({})", self.label())
    }
}

#[derive(Debug, Clone)]
pub struct StorageAccountNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub source: AccountSource,
    pub credential: Option<AuthCredential>,
    /// If false, the account rejects SharedKey; use Bearer directly.
    pub allow_shared_key: bool,
    pub blob_endpoint: String,
    pub queue_endpoint: String,
    /// Container/queue-level SAS: no List Containers/Queues;
    /// the tree shows only the specified resources.
    pub fixed_containers: Option<Vec<String>>,
    pub fixed_queues: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BlobItem {
    pub name: String,
    pub size: u64,
    pub content_type: String,
    pub last_modified: String,
    pub blob_type: String,
    pub etag: String,
}

#[derive(Debug, Clone, Default)]
pub struct BlobListing {
    pub prefixes: Vec<String>,
    pub blobs: Vec<BlobItem>,
}

#[derive(Debug, Clone)]
pub struct BlobPreview {
    pub data: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct QueueInfo {
    pub name: String,
    pub approx_count: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct QueueMessage {
    pub id: String,
    pub insertion_time: String,
    pub expiration_time: String,
    pub dequeue_count: u64,
    pub text: String,
}
