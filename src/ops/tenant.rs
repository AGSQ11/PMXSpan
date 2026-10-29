//! Least-privilege tenant setup, ported from the module's Proxmox user / pool
//! lifecycle (`class.pveuser.php`): a dedicated pool, a `@pve` user, and an
//! ACL granting a role on `/pool/<pool>` only - never cluster-wide.

use crate::api::{Params, PveClient};
use crate::jobs::JobCtx;
use anyhow::{Context, bail};

#[derive(Clone, Debug)]
pub struct TenantSpec {
    pub pool: String,
    pub username: String,
    pub realm: String,
    pub password: String,
    pub email: String,
    pub role: String,
    pub vmids: Vec<u64>,
    pub comment: String,
}

impl Default for TenantSpec {
    fn default() -> Self {
        Self {
            pool: String::new(),
            username: String::new(),
            realm: "pve".into(),
            password: crate::util::generate_password(16),
            email: String::new(),
            role: "PVEVMUser".into(),
            vmids: Vec::new(),
            comment: String::new(),
        }
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub async fn create(client: &PveClient, ctx: &JobCtx, t: TenantSpec) -> anyhow::Result<String> {
    if !valid_id(&t.pool) {
        bail!("pool id may only contain letters, digits, '-', '_' and '.'");
    }
    if !valid_id(&t.username) {
        bail!("username may only contain letters, digits, '-', '_' and '.'");
    }
    if t.password.len() < 8 {
        bail!("Proxmox requires passwords of at least 8 characters");
    }
    let userid = format!("{}@{}", t.username, t.realm);

    let pools = client.get_list("/pools").await?;
    if pools.iter().any(|p| p.get("poolid").and_then(|v| v.as_str()) == Some(t.pool.as_str())) {
        ctx.info(format!("pool {} already exists, reusing it", t.pool));
    } else {
        client.post("/pools", &Params::new().set("poolid", &t.pool).opt("comment", Some(&t.comment))).await.context("creating pool")?;
        ctx.info(format!("created pool {}", t.pool));
    }

    let users = client.get_list("/access/users").await?;
    if users.iter().any(|u| u.get("userid").and_then(|v| v.as_str()) == Some(userid.as_str())) {
        ctx.info(format!("user {userid} exists, resetting its password"));
        client
            .put("/access/password", &Params::new().set("userid", &userid).set("password", &t.password))
            .await
            .context("setting password")?;
    } else {
        let mut p = Params::new().set("userid", &userid).opt("email", Some(&t.email)).opt("comment", Some(&t.comment));
        if t.realm == "pve" {
            p.push("password", &t.password);
        }
        client.post("/access/users", &p).await.context("creating user")?;
        ctx.info(format!("created user {userid}"));
    }

    client
        .put(
            "/access/acl",
            &Params::new().set("path", format!("/pool/{}", t.pool)).set("users", &userid).set("roles", &t.role).set("propagate", 1),
        )
        .await
        .context("granting ACL")?;
    ctx.info(format!("granted {} on /pool/{} to {userid}", t.role, t.pool));

    if !t.vmids.is_empty() {
        let list = t.vmids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        client.put(&format!("/pools/{}", t.pool), &Params::new().set("vms", &list)).await.context("adding guests to pool")?;
        ctx.info(format!("added guests {list} to pool {}", t.pool));
    }
    Ok(format!("tenant {userid} ready (pool {})", t.pool))
}
