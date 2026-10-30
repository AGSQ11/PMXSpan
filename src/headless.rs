//! `pmxspan --run-schedules`: runs every due schedule once, without a window,
//! for cron / Task Scheduler on an always-on machine.
//!
//! The vault's master password comes from `--password-file <file>` or the
//! `PMXSPAN_MASTER_PASSWORD` variable (a vault without one needs neither).
//! Servers using password logins with TOTP cannot run unattended: use an API
//! token for them.

use crate::api::{LoginOutcome, PveClient};
use crate::automation::AutomationStore;
use crate::config::{AppConfig, AppPaths};
use crate::jobs::JobCtx;
use crate::vault::Vault;

pub fn wants_headless() -> bool {
    std::env::args().any(|a| a == "--run-schedules")
}

/// Returns the process exit code: 0 when nothing failed.
pub fn run_schedules(paths: &AppPaths) -> i32 {
    match run(paths) {
        Ok(failed) => i32::from(failed > 0),
        Err(e) => {
            eprintln!("pmxspan: {e:#}");
            tracing::error!("--run-schedules: {e:#}");
            2
        }
    }
}

fn master_password() -> anyhow::Result<zeroize::Zeroizing<String>> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--password-file") {
        let path = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--password-file needs a file"))?;
        let s = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
        return Ok(zeroize::Zeroizing::new(s.trim_end_matches(['\r', '\n']).to_string()));
    }
    Ok(zeroize::Zeroizing::new(std::env::var("PMXSPAN_MASTER_PASSWORD").unwrap_or_default()))
}

fn run(paths: &AppPaths) -> anyhow::Result<usize> {
    let config = AppConfig::load(&paths.config_file());
    let mut store = AutomationStore::load(&paths.automation_file());
    let now = chrono::Local::now();
    let due: Vec<_> = store.schedules.iter().filter(|s| s.due(now)).cloned().collect();
    if due.is_empty() {
        println!("no schedules due");
        return Ok(0);
    }
    let mut vault = Vault::open(&paths.vault_file())?;
    let pw = if vault.is_protected() { master_password()? } else { zeroize::Zeroizing::new(String::new()) };
    vault.unlock(&pw).map_err(|e| anyhow::anyhow!("unlocking the vault: {e} (set PMXSPAN_MASTER_PASSWORD or use --password-file)"))?;

    let mut failed = 0;
    let mut servers: Vec<String> = due.iter().map(|s| s.server_id.clone()).collect();
    servers.sort();
    servers.dedup();
    for sid in servers {
        let jobs: Vec<_> = due.iter().filter(|s| s.server_id == sid).collect();
        let Some(profile) = config.profile(&sid) else {
            for s in &jobs {
                store.record(&s.id, now.timestamp(), &Err(anyhow::anyhow!("server profile no longer exists")));
                failed += 1;
            }
            continue;
        };
        let secret = vault.get(&profile.secret_key()).unwrap_or_default();
        let login = crate::rt::runtime().block_on(PveClient::login(profile, &secret, None));
        let client = match login {
            Ok(LoginOutcome::Ok(c)) => c,
            Ok(LoginOutcome::NeedTotp) => {
                eprintln!("{}: needs a TOTP code; use an API token for unattended runs", profile.display_name());
                failed += jobs.len();
                for s in &jobs {
                    store.record(&s.id, now.timestamp(), &Err(anyhow::anyhow!("login needs TOTP; use an API token")));
                }
                continue;
            }
            Err(e) => {
                eprintln!("{}: {e:#}", profile.display_name());
                failed += jobs.len();
                for s in &jobs {
                    store.record(&s.id, now.timestamp(), &Err(anyhow::anyhow!("login failed: {e:#}")));
                }
                continue;
            }
        };
        for s in jobs {
            let ctx = JobCtx::standalone(format!("schedule {}", s.id));
            let started = chrono::Utc::now().timestamp();
            let r = crate::rt::runtime().block_on(crate::ops::schedule::run(&client, &ctx, s));
            match &r {
                Ok(m) => println!("{} {} ({}): {} -> {m}", s.kind.label(), s.vmid, s.name, s.action.label()),
                Err(e) => {
                    failed += 1;
                    eprintln!("{} {} ({}): {} FAILED: {e:#}", s.kind.label(), s.vmid, s.name, s.action.label());
                }
            }
            tracing::info!("schedule {} on {}: {:?}", s.action.label(), s.vmid, r.as_ref().map_err(|e| format!("{e:#}")));
            store.record(&s.id, started, &r);
        }
    }
    // Another run (or the app) may have changed other schedules meanwhile:
    // merge only the outcomes into the current file.
    let mut current = AutomationStore::load(&paths.automation_file());
    for s in &store.schedules {
        if let Some(c) = current.schedules.iter_mut().find(|c| c.id == s.id) {
            c.last_run = c.last_run.max(s.last_run);
            if s.last_run >= c.last_run {
                c.last_ok = s.last_ok;
                c.last_result = s.last_result.clone();
            }
        }
    }
    current.save()?;
    Ok(failed)
}
