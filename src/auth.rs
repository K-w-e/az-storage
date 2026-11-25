//! Auth Manager: parser delle Connection String e provider Azure CLI.

use crate::model::{AccountSource, AuthCredential, StorageAccountNode};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ARM_RESOURCE: &str = "https://management.azure.com/";
pub const STORAGE_RESOURCE: &str = "https://storage.azure.com/";

const AZURITE_ACCOUNT: &str = "devstoreaccount1";
const AZURITE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

// ---------------------------------------------------------------------------
// Connection String
// ---------------------------------------------------------------------------

/// Converte una connection string (o un SAS URL) in un nodo account "Attached".
pub fn parse_connection_string(profile_name: &str, cs: &str) -> Result<StorageAccountNode> {
    let t = cs.trim();
    if t.starts_with("https://") || t.starts_with("http://") {
        return parse_sas_url(profile_name, t);
    }
    let mut kv: HashMap<String, String> = HashMap::new();
    for part in cs.trim().split(';').filter(|p| !p.trim().is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("segmento non valido: '{part}'"))?;
        kv.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let get = |k: &str| kv.get(k).cloned().filter(|v| !v.is_empty());

    let source = AccountSource::AttachedManual {
        profile_name: profile_name.to_string(),
    };

    if get("usedevelopmentstorage").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        let host = get("developmentstorageproxyuri")
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "http://127.0.0.1".to_string());
        return Ok(StorageAccountNode {
            id: format!("attached:{profile_name}"),
            name: AZURITE_ACCOUNT.to_string(),
            kind: "Azurite".into(),
            source,
            credential: Some(AuthCredential::SharedKey {
                account_name: AZURITE_ACCOUNT.into(),
                account_key: AZURITE_KEY.into(),
            }),
            allow_shared_key: true,
            blob_endpoint: format!("{host}:10000/{AZURITE_ACCOUNT}"),
            queue_endpoint: format!("{host}:10001/{AZURITE_ACCOUNT}"),
            fixed_containers: None,
            fixed_queues: None,
        });
    }

    let protocol = get("defaultendpointsprotocol").unwrap_or_else(|| "https".into());
    let suffix = get("endpointsuffix").unwrap_or_else(|| "core.windows.net".into());
    let account_name = get("accountname");
    let blob_endpoint = get("blobendpoint");
    let queue_endpoint = get("queueendpoint");

    let derive = |svc: &str| -> Option<String> {
        account_name
            .as_ref()
            .map(|n| format!("{protocol}://{n}.{svc}.{suffix}"))
    };
    let blob_endpoint = blob_endpoint
        .or_else(|| derive("blob"))
        .map(|e| e.trim_end_matches('/').to_string())
        .unwrap_or_default();
    let queue_endpoint = queue_endpoint
        .or_else(|| derive("queue"))
        .map(|e| e.trim_end_matches('/').to_string())
        .unwrap_or_default();
    if blob_endpoint.is_empty() && queue_endpoint.is_empty() {
        bail!("AccountName o BlobEndpoint/QueueEndpoint mancanti");
    }

    let name = account_name.clone().unwrap_or_else(|| {
        host_account_name(if blob_endpoint.is_empty() {
            &queue_endpoint
        } else {
            &blob_endpoint
        })
    });

    let credential = if let Some(key) = get("accountkey") {
        Some(AuthCredential::SharedKey {
            account_name: name.clone(),
            account_key: key,
        })
    } else if let Some(sas) = get("sharedaccesssignature") {
        Some(AuthCredential::SasToken(
            sas.trim_start_matches('?').to_string(),
        ))
    } else {
        bail!("AccountKey o SharedAccessSignature mancanti");
    };

    Ok(StorageAccountNode {
        id: format!("attached:{profile_name}"),
        name,
        kind: "Attached".into(),
        source,
        credential,
        allow_shared_key: true,
        blob_endpoint,
        queue_endpoint,
        fixed_containers: None,
        fixed_queues: None,
    })
}

/// SAS URL (come "Attach with SAS URL" di Storage Explorer):
/// - account SAS:   `https://acct.blob.core.windows.net/?sv=..&ss=bq&srt=sco&sig=..`
/// - container SAS: `https://acct.blob.core.windows.net/<container>?sv=..&sr=c&sig=..`
/// - queue SAS:     `https://acct.queue.core.windows.net/<queue>?sv=..&sig=..`
///
/// Supporta anche gli URL path-style di Azurite (`http://127.0.0.1:10000/devstoreaccount1/...`).
pub fn parse_sas_url(profile_name: &str, raw: &str) -> Result<StorageAccountNode> {
    let url = reqwest::Url::parse(raw).map_err(|e| anyhow!("URL non valido: {e}"))?;
    let sas = url
        .query()
        .unwrap_or("")
        .trim_start_matches('?')
        .to_string();
    let params: HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.into_owned()))
        .collect();
    if !params.contains_key("sig") {
        bail!("l'URL non contiene un SAS token (parametro 'sig' mancante)");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("host mancante"))?
        .to_ascii_lowercase();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let mut segs: Vec<String> = url
        .path_segments()
        .map(|s| {
            s.filter(|p| !p.is_empty())
                .map(|p| {
                    urlencoding::decode(p)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| p.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    // Path-style (Azurite / IP): il primo segmento è il nome account.
    let path_style = host.parse::<std::net::IpAddr>().is_ok() || host == "localhost";
    let (account, base) = if path_style {
        if segs.is_empty() {
            bail!("URL path-style senza nome account");
        }
        let acct = segs.remove(0);
        (
            acct.clone(),
            format!("{}://{host}{port}/{acct}", url.scheme()),
        )
    } else {
        let acct = host.split('.').next().unwrap_or("account").to_string();
        (acct, format!("{}://{host}{port}", url.scheme()))
    };

    #[derive(PartialEq)]
    enum Svc {
        Blob,
        Queue,
    }
    let svc = if path_style {
        match url.port() {
            Some(10001) => Svc::Queue,
            _ => Svc::Blob,
        }
    } else if host.contains(".queue.") {
        Svc::Queue
    } else if host.contains(".blob.") || host.contains(".dfs.") {
        Svc::Blob
    } else {
        bail!("servizio non riconosciuto dall'host '{host}' (attesi *.blob.* o *.queue.*)");
    };
    let base = base.replace(".dfs.", ".blob.");

    let sr = params.get("sr").map(String::as_str).unwrap_or("");
    if matches!(sr, "b" | "bs" | "bv" | "d") || (svc == Svc::Blob && segs.len() > 1) {
        bail!(
            "SAS a livello di singolo blob/directory non supportato: serve un SAS di account o di container"
        );
    }
    let resource = segs.first().cloned();

    // Endpoint dell'altro servizio derivabile solo per account SAS non path-style.
    let other = |from: &str, to: &str| -> String {
        let ss = params.get("ss").map(String::as_str).unwrap_or("");
        let svc_char = if to == "queue" { 'q' } else { 'b' };
        if resource.is_none() && !path_style && ss.contains(svc_char) {
            base.replacen(&format!(".{from}."), &format!(".{to}."), 1)
        } else {
            String::new()
        }
    };
    let (blob_endpoint, queue_endpoint) = match svc {
        Svc::Blob => (base.clone(), other("blob", "queue")),
        Svc::Queue => (other("queue", "blob"), base.clone()),
    };

    let expired = params
        .get("se")
        .and_then(|se| chrono::DateTime::parse_from_rfc3339(se).ok())
        .is_some_and(|se| se < chrono::Utc::now());
    let scope = match (&svc, &resource) {
        (_, None) => "account",
        (Svc::Blob, Some(_)) => "container",
        (Svc::Queue, Some(_)) => "queue",
    };
    let kind = if expired {
        format!("SAS {scope} · EXPIRED")
    } else {
        format!("SAS {scope}")
    };

    Ok(StorageAccountNode {
        id: format!("attached:{profile_name}"),
        name: account,
        kind,
        source: AccountSource::AttachedManual {
            profile_name: profile_name.to_string(),
        },
        credential: Some(AuthCredential::SasToken(sas)),
        allow_shared_key: true,
        blob_endpoint,
        queue_endpoint,
        fixed_containers: match (&svc, &resource) {
            (Svc::Blob, Some(c)) => Some(vec![c.clone()]),
            _ => None,
        },
        fixed_queues: match (&svc, &resource) {
            (Svc::Queue, Some(q)) => Some(vec![q.clone()]),
            _ => None,
        },
    })
}

fn host_account_name(endpoint: &str) -> String {
    endpoint
        .split("://")
        .nth(1)
        .and_then(|h| h.split(['.', '/', ':']).next())
        .unwrap_or("account")
        .to_string()
}

// ---------------------------------------------------------------------------
// Azure CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CliToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl CliToken {
    pub fn is_fresh(&self) -> bool {
        self.expires_at > SystemTime::now() + Duration::from_secs(120)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawToken {
    access_token: String,
    #[serde(rename = "expires_on")]
    expires_on_epoch: Option<u64>,
}

async fn run_az(args: &[&str]) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new("az")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("impossibile eseguire `az` (Azure CLI installata e nel PATH?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("errore sconosciuto");
        bail!("az {}: {}", args.first().unwrap_or(&""), first.trim());
    }
    Ok(out.stdout)
}

pub async fn get_access_token(resource: &str, tenant: Option<&str>) -> Result<CliToken> {
    let mut args = vec![
        "account",
        "get-access-token",
        "--resource",
        resource,
        "-o",
        "json",
    ];
    if let Some(t) = tenant {
        args.extend(["--tenant", t]);
    }
    let raw: RawToken = serde_json::from_slice(&run_az(&args).await?)
        .context("output di `az account get-access-token` non valido")?;
    let expires_at = raw
        .expires_on_epoch
        .map(|e| UNIX_EPOCH + Duration::from_secs(e))
        .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(5 * 60));
    Ok(CliToken {
        token: raw.access_token,
        expires_at,
    })
}

/// Restituisce lo user principal della sessione `az login` attiva.
pub async fn current_user() -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_slice(&run_az(&["account", "show", "-o", "json"]).await?)?;
    Ok(v["user"]["name"].as_str().unwrap_or("unknown").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_connection_string() {
        let n = parse_connection_string(
            "p",
            "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=a2V5;EndpointSuffix=core.windows.net",
        )
        .unwrap();
        assert_eq!(n.name, "acct");
        assert_eq!(n.blob_endpoint, "https://acct.blob.core.windows.net");
        assert_eq!(n.queue_endpoint, "https://acct.queue.core.windows.net");
        assert!(matches!(
            n.credential,
            Some(AuthCredential::SharedKey { .. })
        ));
    }

    #[test]
    fn parses_container_sas_url() {
        let n = parse_connection_string(
            "c",
            "https://acct.blob.core.windows.net/my-container?sp=racwdli&st=2024-12-04T10:10:17Z&se=2099-12-04T18:10:17Z&spr=https&sv=2022-11-02&sr=c&sig=ab%2Fc%3D",
        )
        .unwrap();
        assert_eq!(n.name, "acct");
        assert_eq!(n.blob_endpoint, "https://acct.blob.core.windows.net");
        assert!(n.queue_endpoint.is_empty());
        assert_eq!(n.fixed_containers, Some(vec!["my-container".to_string()]));
        assert_eq!(n.kind, "SAS container");
        // Il token resta codificato così com'era nell'URL.
        assert!(
            matches!(n.credential, Some(AuthCredential::SasToken(ref s)) if s.ends_with("sig=ab%2Fc%3D") && s.starts_with("sp="))
        );
    }

    #[test]
    fn parses_account_and_queue_sas_urls() {
        let n = parse_connection_string("a", "https://acct.blob.core.windows.net/?sv=2022-11-02&ss=bq&srt=sco&se=2020-01-01T00:00:00Z&sig=x").unwrap();
        assert_eq!(n.queue_endpoint, "https://acct.queue.core.windows.net");
        assert!(n.fixed_containers.is_none());
        assert_eq!(n.kind, "SAS account · EXPIRED");
        let q = parse_connection_string(
            "q",
            "https://acct.queue.core.windows.net/orders?sv=2022-11-02&sig=x",
        )
        .unwrap();
        assert_eq!(q.fixed_queues, Some(vec!["orders".to_string()]));
        assert!(q.blob_endpoint.is_empty());
        let z = parse_connection_string(
            "z",
            "http://127.0.0.1:10000/devstoreaccount1/demo?sv=2022-11-02&sr=c&sig=x",
        )
        .unwrap();
        assert_eq!(z.blob_endpoint, "http://127.0.0.1:10000/devstoreaccount1");
        assert_eq!(z.fixed_containers, Some(vec!["demo".to_string()]));
        assert!(
            parse_connection_string(
                "b",
                "https://acct.blob.core.windows.net/c/file.txt?sr=b&sig=x"
            )
            .is_err()
        );
        assert!(parse_connection_string("n", "https://acct.blob.core.windows.net/c").is_err());
    }

    #[test]
    fn parses_azurite() {
        let n = parse_connection_string("dev", "UseDevelopmentStorage=true").unwrap();
        assert_eq!(n.blob_endpoint, "http://127.0.0.1:10000/devstoreaccount1");
        assert_eq!(n.queue_endpoint, "http://127.0.0.1:10001/devstoreaccount1");
    }

    #[test]
    fn parses_sas() {
        let n = parse_connection_string(
            "s",
            "BlobEndpoint=https://x.blob.core.windows.net/;SharedAccessSignature=?sv=2022&sig=abc",
        )
        .unwrap();
        assert_eq!(n.name, "x");
        assert!(n.queue_endpoint.is_empty());
        assert!(
            matches!(n.credential, Some(AuthCredential::SasToken(ref s)) if s == "sv=2022&sig=abc")
        );
    }
}
