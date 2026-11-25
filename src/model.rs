//! Modello dati condiviso tra UI e backend worker.

#[derive(Debug, Clone)]
#[allow(dead_code)] // campi da specifica, non tutti usati dalla UI
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
    /// Token ottenuto da Azure CLI per ARM / Data-Plane
    BearerToken(String),
    /// Account Key o SAS estratta da ARM listKeys o da Connection String
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
    // Mai stampare segreti.
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
    /// Se false, l'account rifiuta SharedKey: usare direttamente Bearer.
    pub allow_shared_key: bool,
    pub blob_endpoint: String,
    pub queue_endpoint: String,
    /// SAS a livello di container/coda: niente List Containers/Queues,
    /// l'albero mostra solo le risorse indicate.
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
