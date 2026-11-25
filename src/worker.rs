//! Async Backend Worker: riceve `Command` dalla UI, esegue I/O in task
//! `tokio::spawn` e restituisce `Event`.

use crate::arm;
use crate::auth::{self, ARM_RESOURCE, CliToken, STORAGE_RESOURCE};
use crate::model::*;
use crate::storage::{StorageClient, Target};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Blob,
    Queue,
}

#[derive(Debug)]
pub enum Command {
    DiscoverCli,
    LoadAccounts(Subscription),
    ListContainers(StorageAccountNode),
    ListQueues(StorageAccountNode),
    ListBlobs {
        account: StorageAccountNode,
        container: String,
        prefix: String,
    },
    PreviewBlob {
        account: StorageAccountNode,
        container: String,
        blob: String,
    },
    DeleteBlob {
        account: StorageAccountNode,
        container: String,
        blob: String,
        prefix: String,
    },
    PeekMessages {
        account: StorageAccountNode,
        queue: String,
    },
    DeleteMessage {
        account: StorageAccountNode,
        queue: String,
        id: String,
    },
    ClearQueue {
        account: StorageAccountNode,
        queue: String,
    },
    Download {
        account: StorageAccountNode,
        container: String,
        spec: DownloadSpec,
        dest: PathBuf,
        overwrite: bool,
    },
    Upload {
        account: StorageAccountNode,
        container: String,
        prefix: String,
        src: PathBuf,
        overwrite: bool,
    },
    CancelTransfers,
}

/// Oggetto da scaricare: singolo blob o tutto ciò che sta sotto un prefisso.
#[derive(Debug, Clone)]
pub enum DownloadSpec {
    Blob(String),
    Prefix(String),
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub upload: bool,
    pub label: String,
    pub current: String,
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
}

pub type Res<T> = std::result::Result<T, String>;

#[derive(Debug)]
pub enum Event {
    CliUser(Res<String>),
    Subscriptions(Res<Vec<Subscription>>),
    Accounts {
        sub_id: String,
        result: Res<Vec<StorageAccountNode>>,
    },
    Credential {
        account_id: String,
        cred: AuthCredential,
    },
    Containers {
        account_id: String,
        result: Res<Vec<String>>,
    },
    Queues {
        account_id: String,
        result: Res<Vec<QueueInfo>>,
    },
    Blobs {
        account_id: String,
        container: String,
        prefix: String,
        result: Res<BlobListing>,
    },
    BlobPreview {
        account_id: String,
        container: String,
        blob: String,
        result: Res<BlobPreview>,
    },
    Messages {
        account_id: String,
        queue: String,
        result: Res<Vec<QueueMessage>>,
    },
    Transfer {
        id: u64,
        progress: TransferProgress,
    },
    TransferDone {
        id: u64,
        account_id: String,
        container: String,
        upload: bool,
        result: Res<String>,
    },
    Info(String),
    Error(String),
}

struct Worker {
    http: reqwest::Client,
    storage: StorageClient,
    tx: UnboundedSender<Event>,
    /// Cache token CLI: chiave (resource, tenant).
    tokens: Mutex<HashMap<(String, Option<String>), CliToken>>,
    /// Client senza timeout totale per upload/download di file grandi.
    xfer: StorageClient,
    next_id: AtomicU64,
    cancels: std::sync::Mutex<HashMap<u64, Arc<AtomicBool>>>,
}

fn s<T>(r: Result<T>) -> Res<T> {
    r.map_err(|e| format!("{e:#}"))
}

pub async fn run(
    mut rx: UnboundedReceiver<Command>,
    tx: UnboundedSender<Event>,
    busy: Arc<AtomicUsize>,
) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client");
    let xfer_http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client");
    let w = Arc::new(Worker {
        xfer: StorageClient::new(xfer_http),
        next_id: AtomicU64::new(1),
        cancels: std::sync::Mutex::new(HashMap::new()),
        storage: StorageClient::new(http.clone()),
        http,
        tx,
        tokens: Mutex::new(HashMap::new()),
    });
    while let Some(cmd) = rx.recv().await {
        let w = w.clone();
        let busy = busy.clone();
        busy.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            w.handle(cmd).await;
            busy.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

impl Worker {
    fn emit(&self, e: Event) {
        let _ = self.tx.send(e);
    }

    async fn token(&self, resource: &str, tenant: Option<&str>) -> Result<String> {
        let key = (resource.to_string(), tenant.map(str::to_string));
        if let Some(t) = self.tokens.lock().await.get(&key).filter(|t| t.is_fresh()) {
            return Ok(t.token.clone());
        }
        let t = auth::get_access_token(resource, tenant).await?;
        let tok = t.token.clone();
        self.tokens.lock().await.insert(key, t);
        Ok(tok)
    }

    /// Risolve la credenziale data-plane: listKeys (key1) → fallback Bearer.
    async fn credential(&self, acct: &StorageAccountNode) -> Result<AuthCredential> {
        if let Some(c) = &acct.credential {
            return Ok(c.clone());
        }
        let AccountSource::AzureSubscription { tenant_id, .. } = &acct.source else {
            bail!("nessuna credenziale per {}", acct.name);
        };
        let tenant = tenant_id.as_deref();
        let mut cred = None;
        if acct.allow_shared_key {
            let arm_tok = self.token(ARM_RESOURCE, tenant).await?;
            match arm::list_keys(&self.http, &arm_tok, &acct.id).await {
                Ok(key) => {
                    cred = Some(AuthCredential::SharedKey {
                        account_name: acct.name.clone(),
                        account_key: key,
                    })
                }
                Err(e) => self.emit(Event::Info(format!(
                    "listKeys non disponibile per {} ({e:#}); uso Entra ID",
                    acct.name
                ))),
            }
        }
        let cred = match cred {
            Some(c) => c,
            None => AuthCredential::BearerToken(self.token(STORAGE_RESOURCE, tenant).await?),
        };
        self.emit(Event::Credential {
            account_id: acct.id.clone(),
            cred: cred.clone(),
        });
        Ok(cred)
    }

    async fn with_target<T, F, Fut>(
        &self,
        acct: &StorageAccountNode,
        svc: Service,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce(StorageClient, String, AuthCredential) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let endpoint = match svc {
            Service::Blob => &acct.blob_endpoint,
            Service::Queue => &acct.queue_endpoint,
        };
        if endpoint.is_empty() {
            bail!("servizio non disponibile su {}", acct.name);
        }
        let cred = self.credential(acct).await?;
        f(self.storage.clone(), endpoint.clone(), cred).await
    }

    async fn handle(&self, cmd: Command) {
        match cmd {
            Command::DiscoverCli => {
                let user = auth::current_user().await;
                let ok = user.is_ok();
                self.emit(Event::CliUser(s(user)));
                if !ok {
                    return;
                }
                let subs = async {
                    let tok = self.token(ARM_RESOURCE, None).await?;
                    arm::list_subscriptions(&self.http, &tok).await
                }
                .await;
                self.emit(Event::Subscriptions(s(subs)));
            }
            Command::LoadAccounts(sub) => {
                let r = async {
                    let tok = self.token(ARM_RESOURCE, sub.tenant_id.as_deref()).await?;
                    arm::list_storage_accounts(&self.http, &tok, &sub).await
                }
                .await;
                self.emit(Event::Accounts {
                    sub_id: sub.id,
                    result: s(r),
                });
            }
            Command::ListContainers(acct) if acct.fixed_containers.is_some() => {
                self.emit(Event::Containers {
                    account_id: acct.id.clone(),
                    result: Ok(acct.fixed_containers.clone().unwrap_or_default()),
                });
            }
            Command::ListQueues(acct) if acct.fixed_queues.is_some() => {
                let names = acct.fixed_queues.clone().unwrap_or_default();
                let r = self
                    .with_target(&acct, Service::Queue, |c, ep, cred| async move {
                        Ok(c.queues_with_counts(
                            &Target {
                                endpoint: &ep,
                                cred: &cred,
                            },
                            names,
                        )
                        .await)
                    })
                    .await;
                self.emit(Event::Queues {
                    account_id: acct.id,
                    result: s(r),
                });
            }
            Command::ListContainers(acct) => {
                let r = self
                    .with_target(&acct, Service::Blob, |c, ep, cred| async move {
                        c.list_containers(&Target {
                            endpoint: &ep,
                            cred: &cred,
                        })
                        .await
                    })
                    .await;
                self.emit(Event::Containers {
                    account_id: acct.id,
                    result: s(r),
                });
            }
            Command::ListQueues(acct) => {
                let r = self
                    .with_target(&acct, Service::Queue, |c, ep, cred| async move {
                        c.list_queues(&Target {
                            endpoint: &ep,
                            cred: &cred,
                        })
                        .await
                    })
                    .await;
                self.emit(Event::Queues {
                    account_id: acct.id,
                    result: s(r),
                });
            }
            Command::ListBlobs {
                account,
                container,
                prefix,
            } => self.list_blobs(account, container, prefix).await,
            Command::PreviewBlob {
                account,
                container,
                blob,
            } => {
                let (cn, bn) = (container.clone(), blob.clone());
                let r = self
                    .with_target(&account, Service::Blob, |c, ep, cred| async move {
                        c.preview_blob(
                            &Target {
                                endpoint: &ep,
                                cred: &cred,
                            },
                            &cn,
                            &bn,
                        )
                        .await
                    })
                    .await;
                self.emit(Event::BlobPreview {
                    account_id: account.id,
                    container,
                    blob,
                    result: s(r),
                });
            }
            Command::DeleteBlob {
                account,
                container,
                blob,
                prefix,
            } => {
                let (cn, bn) = (container.clone(), blob.clone());
                let r = self
                    .with_target(&account, Service::Blob, |c, ep, cred| async move {
                        c.delete_blob(
                            &Target {
                                endpoint: &ep,
                                cred: &cred,
                            },
                            &cn,
                            &bn,
                        )
                        .await
                    })
                    .await;
                match r {
                    Ok(()) => self.emit(Event::Info(format!("Eliminato {container}/{blob}"))),
                    Err(e) => self.emit(Event::Error(format!("Delete blob fallito: {e:#}"))),
                }
                self.list_blobs(account, container, prefix).await;
            }
            Command::PeekMessages { account, queue } => self.peek(account, queue).await,
            Command::DeleteMessage { account, queue, id } => {
                let (qn, idc) = (queue.clone(), id.clone());
                let r = self
                    .with_target(&account, Service::Queue, |c, ep, cred| async move {
                        c.delete_message(
                            &Target {
                                endpoint: &ep,
                                cred: &cred,
                            },
                            &qn,
                            &idc,
                        )
                        .await
                    })
                    .await;
                match r {
                    Ok(()) => self.emit(Event::Info(format!("Messaggio {id} eliminato"))),
                    Err(e) => self.emit(Event::Error(format!("Delete message fallito: {e:#}"))),
                }
                self.peek(account, queue).await;
            }
            Command::Download {
                account,
                container,
                spec,
                dest,
                overwrite,
            } => {
                self.download(account, container, spec, dest, overwrite)
                    .await
            }
            Command::Upload {
                account,
                container,
                prefix,
                src,
                overwrite,
            } => {
                self.upload(account, container, prefix, src, overwrite)
                    .await
            }
            Command::CancelTransfers => {
                for c in self.cancels.lock().unwrap().values() {
                    c.store(true, Ordering::Relaxed);
                }
            }
            Command::ClearQueue { account, queue } => {
                let qn = queue.clone();
                let r = self
                    .with_target(&account, Service::Queue, |c, ep, cred| async move {
                        c.clear_queue(
                            &Target {
                                endpoint: &ep,
                                cred: &cred,
                            },
                            &qn,
                        )
                        .await
                    })
                    .await;
                match r {
                    Ok(()) => self.emit(Event::Info(format!("Coda {queue} svuotata"))),
                    Err(e) => self.emit(Event::Error(format!("Clear queue fallito: {e:#}"))),
                }
                self.peek(account, queue).await;
            }
        }
    }

    async fn list_blobs(&self, account: StorageAccountNode, container: String, prefix: String) {
        let (cn, pf) = (container.clone(), prefix.clone());
        let r = self
            .with_target(&account, Service::Blob, |c, ep, cred| async move {
                c.list_blobs(
                    &Target {
                        endpoint: &ep,
                        cred: &cred,
                    },
                    &cn,
                    &pf,
                )
                .await
            })
            .await;
        self.emit(Event::Blobs {
            account_id: account.id,
            container,
            prefix,
            result: s(r),
        });
    }

    async fn peek(&self, account: StorageAccountNode, queue: String) {
        let qn = queue.clone();
        let r = self
            .with_target(&account, Service::Queue, |c, ep, cred| async move {
                c.peek_messages(
                    &Target {
                        endpoint: &ep,
                        cred: &cred,
                    },
                    &qn,
                )
                .await
            })
            .await;
        self.emit(Event::Messages {
            account_id: account.id,
            queue,
            result: s(r),
        });
    }
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

/// Emissione progress limitata (~10/s) per non saturare il canale UI.
struct Reporter<'a> {
    w: &'a Worker,
    id: u64,
    p: TransferProgress,
    last: Instant,
}

impl Reporter<'_> {
    fn add(&mut self, n: u64) {
        self.p.done += n;
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.flush();
        }
    }
    fn flush(&mut self) {
        self.last = Instant::now();
        self.w.emit(Event::Transfer {
            id: self.id,
            progress: self.p.clone(),
        });
    }
}

/// Converte un path relativo di blob in path locale sicuro (no `..`/assoluti).
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            return None;
        }
        out.push(part);
    }
    (out != base).then_some(out)
}

fn walk_dir(root: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".DS_Store" {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(&e.path())
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((e.path(), rel));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

impl Worker {
    fn begin_transfer(&self) -> (u64, Arc<AtomicBool>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(id, flag.clone());
        (id, flag)
    }

    fn end_transfer(
        &self,
        id: u64,
        account_id: String,
        container: String,
        upload: bool,
        r: Result<String>,
    ) {
        self.cancels.lock().unwrap().remove(&id);
        self.emit(Event::TransferDone {
            id,
            account_id,
            container,
            upload,
            result: s(r),
        });
    }

    async fn blob_target(&self, acct: &StorageAccountNode) -> Result<(String, AuthCredential)> {
        if acct.blob_endpoint.is_empty() {
            bail!("Blob service non disponibile su {}", acct.name);
        }
        Ok((acct.blob_endpoint.clone(), self.credential(acct).await?))
    }

    async fn download(
        &self,
        account: StorageAccountNode,
        container: String,
        spec: DownloadSpec,
        dest: PathBuf,
        overwrite: bool,
    ) {
        let (id, cancel) = self.begin_transfer();
        let r = async {
            let (ep, cred) = self.blob_target(&account).await?;
            let t = Target {
                endpoint: &ep,
                cred: &cred,
            };
            // (blob, destinazione locale, size)
            let (label, files): (String, Vec<(String, PathBuf, u64)>) = match &spec {
                DownloadSpec::Blob(name) => {
                    let size = self
                        .storage
                        .blob_size(&t, &container, name)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("blob {name} non trovato"))?;
                    (name.clone(), vec![(name.clone(), dest.clone(), size)])
                }
                DownloadSpec::Prefix(prefix) => {
                    let blobs = self.storage.list_blobs_flat(&t, &container, prefix).await?;
                    let files = blobs
                        .into_iter()
                        .filter(|b| !b.name.ends_with('/'))
                        .filter_map(|b| {
                            safe_join(&dest, &b.name[prefix.len()..]).map(|p| (b.name, p, b.size))
                        })
                        .collect();
                    (format!("{container}/{prefix}"), files)
                }
            };
            let mut rep = Reporter {
                w: self,
                id,
                last: Instant::now(),
                p: TransferProgress {
                    upload: false,
                    label,
                    current: String::new(),
                    done: 0,
                    total: files.iter().map(|f| f.2).sum(),
                    files_done: 0,
                    files_total: files.len(),
                },
            };
            rep.flush();
            let (mut ok, mut skipped) = (0usize, 0usize);
            for (name, path, size) in &files {
                if !overwrite && path.exists() {
                    skipped += 1;
                    rep.p.total -= size;
                } else {
                    rep.p.current = name.clone();
                    self.xfer
                        .download_blob(&t, &container, name, path, &cancel, |n| rep.add(n))
                        .await
                        .with_context(|| name.clone())?;
                    ok += 1;
                }
                rep.p.files_done += 1;
                rep.flush();
            }
            let mut msg = match &spec {
                DownloadSpec::Blob(_) => format!("Downloaded to {}", dest.display()),
                DownloadSpec::Prefix(_) => format!("Downloaded {ok} file(s) to {}", dest.display()),
            };
            if skipped > 0 {
                msg.push_str(&format!(" · {skipped} skipped (already exist)"));
            }
            Ok(msg)
        }
        .await;
        self.end_transfer(id, account.id, container, false, r);
    }

    async fn upload(
        &self,
        account: StorageAccountNode,
        container: String,
        prefix: String,
        src: PathBuf,
        overwrite: bool,
    ) {
        let (id, cancel) = self.begin_transfer();
        let r = async {
            let (ep, cred) = self.blob_target(&account).await?;
            let t = Target {
                endpoint: &ep,
                cred: &cred,
            };
            let base = src
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let files: Vec<(PathBuf, String)> = if src.is_dir() {
                let root = src.clone();
                tokio::task::spawn_blocking(move || walk_dir(&root))
                    .await??
                    .into_iter()
                    .map(|(p, rel)| (p, format!("{prefix}{base}/{rel}")))
                    .collect()
            } else {
                vec![(src.clone(), format!("{prefix}{base}"))]
            };
            let mut total = 0;
            for (p, _) in &files {
                total += tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
            }
            let mut rep = Reporter {
                w: self,
                id,
                last: Instant::now(),
                p: TransferProgress {
                    upload: true,
                    label: base.clone(),
                    current: String::new(),
                    done: 0,
                    total,
                    files_done: 0,
                    files_total: files.len(),
                },
            };
            rep.flush();
            let (mut ok, mut skipped) = (0usize, 0usize);
            for (path, blob) in &files {
                rep.p.current = blob.clone();
                let size = tokio::fs::metadata(path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                let uploaded = self
                    .xfer
                    .upload_file(&t, &container, blob, path, overwrite, &cancel, |n| {
                        rep.add(n)
                    })
                    .await
                    .with_context(|| path.display().to_string())?;
                if uploaded {
                    ok += 1;
                } else {
                    skipped += 1;
                    rep.p.total = rep.p.total.saturating_sub(size);
                }
                rep.p.files_done += 1;
                rep.flush();
            }
            let mut msg = format!("Uploaded {ok} file(s) to {container}/{prefix}");
            if skipped > 0 {
                msg.push_str(&format!(
                    " · {skipped} skipped (already exist, Ctrl-O to overwrite)"
                ));
            }
            Ok(msg)
        }
        .await;
        self.end_transfer(id, account.id, container, true, r);
    }
}
