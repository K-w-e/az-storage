//! ARM Discovery: Subscriptions, Storage Accounts e listKeys.

use crate::model::{AccountSource, StorageAccountNode, Subscription};
use anyhow::{Result, bail};
use serde_json::Value;

const ARM: &str = "https://management.azure.com";

async fn get_json(http: &reqwest::Client, token: &str, url: &str) -> Result<Value> {
    let resp = http.get(url).bearer_auth(token).send().await?;
    parse(resp).await
}

async fn parse(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let code = v["error"]["code"].as_str().unwrap_or("");
        let msg = v["error"]["message"].as_str().unwrap_or(body.as_str());
        bail!("ARM HTTP {}: {} {}", status.as_u16(), code, msg);
    }
    Ok(serde_json::from_str(&body)?)
}

/// GET con paginazione `nextLink`.
async fn get_paged(http: &reqwest::Client, token: &str, url: String) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut next = Some(url);
    while let Some(url) = next.take() {
        let v = get_json(http, token, &url).await?;
        if let Some(items) = v["value"].as_array() {
            out.extend(items.iter().cloned());
        }
        next = v["nextLink"].as_str().map(str::to_string);
    }
    Ok(out)
}

pub async fn list_subscriptions(http: &reqwest::Client, token: &str) -> Result<Vec<Subscription>> {
    let items = get_paged(
        http,
        token,
        format!("{ARM}/subscriptions?api-version=2022-12-01"),
    )
    .await?;
    let mut subs: Vec<Subscription> = items
        .iter()
        .filter(|s| s["state"].as_str().is_none_or(|st| st != "Disabled"))
        .map(|s| Subscription {
            id: s["subscriptionId"].as_str().unwrap_or_default().to_string(),
            name: s["displayName"].as_str().unwrap_or_default().to_string(),
            tenant_id: s["tenantId"].as_str().map(str::to_string),
        })
        .collect();
    subs.sort_by_key(|s| s.name.to_lowercase());
    Ok(subs)
}

pub async fn list_storage_accounts(
    http: &reqwest::Client,
    token: &str,
    sub: &Subscription,
) -> Result<Vec<StorageAccountNode>> {
    let url = format!(
        "{ARM}/subscriptions/{}/providers/Microsoft.Storage/storageAccounts?api-version=2023-01-01",
        sub.id
    );
    let items = get_paged(http, token, url).await?;
    let mut out: Vec<StorageAccountNode> = items
        .iter()
        .map(|a| {
            let id = a["id"].as_str().unwrap_or_default().to_string();
            let ep = |svc: &str| {
                a["properties"]["primaryEndpoints"][svc]
                    .as_str()
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string()
            };
            StorageAccountNode {
                name: a["name"].as_str().unwrap_or_default().to_string(),
                kind: a["kind"].as_str().unwrap_or_default().to_string(),
                source: AccountSource::AzureSubscription {
                    subscription_id: sub.id.clone(),
                    subscription_name: sub.name.clone(),
                    resource_group: resource_group_of(&id),
                    tenant_id: sub.tenant_id.clone(),
                },
                credential: None,
                allow_shared_key: a["properties"]["allowSharedKeyAccess"].as_bool() != Some(false),
                fixed_containers: None,
                fixed_queues: None,
                blob_endpoint: ep("blob"),
                queue_endpoint: ep("queue"),
                id,
            }
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    Ok(out)
}

/// `POST .../listKeys` → valore di `key1` (o della prima chiave disponibile).
pub async fn list_keys(http: &reqwest::Client, token: &str, account_id: &str) -> Result<String> {
    let url = format!("{ARM}{account_id}/listKeys?api-version=2023-01-01");
    let resp = http
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_LENGTH, "0")
        .send()
        .await?;
    let v = parse(resp).await?;
    let keys = v["keys"].as_array().cloned().unwrap_or_default();
    keys.iter()
        .find(|k| k["keyName"].as_str() == Some("key1"))
        .or_else(|| keys.first())
        .and_then(|k| k["value"].as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("listKeys: nessuna chiave restituita"))
}

fn resource_group_of(id: &str) -> String {
    let parts: Vec<&str> = id.split('/').collect();
    parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("resourceGroups"))
        .and_then(|i| parts.get(i + 1))
        .unwrap_or(&"")
        .to_string()
}
