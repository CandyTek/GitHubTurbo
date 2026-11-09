use super::*;
pub fn local_host(header: &str, listen: SocketAddr) -> bool {
    header.eq_ignore_ascii_case(&listen.to_string())
        || header.eq_ignore_ascii_case(&format!("localhost:{}", listen.port()))
}
pub fn write_allowed(headers: &HashMap<String, String>, listen: SocketAddr) -> bool {
    if headers
        .get("x-github-turbo-request")
        .is_none_or(|v| v != "1")
    {
        return false;
    }
    if let Some(origin) = headers.get("origin") {
        return origin == &format!("http://{listen}")
            || origin == &format!("http://localhost:{}", listen.port());
    }
    true
}
pub async fn status(state: &State) -> serde_json::Value {
    let routes = state.routes.lock().await;
    let entries:Vec<_>=routes.iter().map(|(host,cached)|serde_json::json!({"host":host.strip_prefix("bulk:").unwrap_or(host),"profile":if host.starts_with("bulk:"){"bulk"}else{"latency"},"route":cached.route,"age_seconds":cached.at.elapsed().as_secs()})).collect();
    drop(routes);
    let c = &state.counters;
    serde_json::json!({"name":"GitHubTurbo","version":env!("CARGO_PKG_VERSION"),"transparent":state.config.transparent.enabled,"listen":state.config.listen,"upstream":state.config.upstream,"mode":state.config.mode,"host_modes":state.config.host_modes,"optimization":state.optimization(),"uptime_seconds":state.started.elapsed().as_secs(),"accepted":c.accepted.load(Ordering::Relaxed),"completed":c.completed.load(Ordering::Relaxed),"failures":c.failures.load(Ordering::Relaxed),"uploaded_bytes":c.uploaded.load(Ordering::Relaxed),"downloaded_bytes":c.downloaded.load(Ordering::Relaxed),"routes":entries,"pool":state.pool.snapshot().await,"health":state.health.snapshot(),"cache":state.content_cache.snapshot(),"range_memo":state.download_memo.snapshot(),"warm_tls":state.warm.snapshot().await,"split_active":2-state.split_slots.available_permits(),"host_sources":state.sources.snapshot(),"transfer_history":state.performance.snapshot()})
}
pub async fn write(path: &str, stream: &mut TcpStream, state: &Arc<State>) -> Result<()> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    match path {
        "/api/options" => {
            let _write = state.option_update.lock().await;
            let mut next = state.optimization();
            for field in query.split('&').filter(|s| !s.is_empty()) {
                let Some((key, value)) = field.split_once('=') else {
                    return respond(stream, "400 Bad Request", "text/plain", "Invalid option\n")
                        .await;
                };
                let value = match value {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => {
                        return respond(
                            stream,
                            "400 Bad Request",
                            "text/plain",
                            "Invalid boolean\n",
                        )
                        .await
                    }
                };
                match key {
                    "http2" => next.http2 = value,
                    "public_cache" => next.public_cache = value,
                    "segmented_downloads" => next.segmented_downloads = value,
                    "seed_routes" => next.seed_routes = value,
                    "range_memo" => next.range_memo = value,
                    "probe_reuse" => next.probe_reuse = value,
                    "range_pipeline" => next.range_pipeline = value,
                    "adaptive_split" => next.adaptive_split = value,
                    "host_sources" => next.host_sources = value,
                    "adaptive_routing" => next.adaptive_routing = value,
                    _ => {
                        return respond(stream, "400 Bad Request", "text/plain", "Unknown option\n")
                            .await
                    }
                }
            }
            let path = state.config_file.lock().unwrap().clone();
            if let Some(path) = path {
                let next = next.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let bytes = std::fs::read(&path)?;
                    if bytes.len() > 65536 {
                        return Err(err("configuration too large"));
                    }
                    let mut config: serde_json::Value = serde_json::from_slice(&bytes)?;
                    config["optimization"] = serde_json::to_value(next)?;
                    let temp = path.with_extension("json.tmp");
                    std::fs::write(&temp, serde_json::to_vec_pretty(&config)?)?;
                    std::fs::rename(temp, path)?;
                    Ok(())
                })
                .await??;
            }
            *state.options.write().unwrap() = next;
            state.routes.lock().await.clear();
            state.pool.clear().await;
            state.warm.clear().await;
            respond(stream, "200 OK", "application/json", "{\"ok\":true}").await
        }
        "/api/reset" => {
            state.pool.clear().await;
            state.warm.clear().await;
            state.routes.lock().await.clear();
            state.health.clear();
            state.performance.clear();
            state.content_cache.clear();
            state.download_memo.clear();
            let _ = state.persist_routes().await;
            respond(stream, "200 OK", "application/json", "{\"ok\":true}").await
        }
        "/api/cache/clear" => {
            state.content_cache.clear();
            respond(stream, "200 OK", "application/json", "{\"ok\":true}").await
        }
        "/api/stop" => {
            if !state.config.transparent.enabled {
                return respond(
                    stream,
                    "409 Conflict",
                    "application/json",
                    "{\"error\":\"Use Ctrl+C for standalone mode\"}",
                )
                .await;
            }
            respond(
                stream,
                "200 OK",
                "application/json",
                "{\"ok\":true,\"message\":\"Restoring settings\"}",
            )
            .await?;
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let script = std::env::current_exe()?.with_file_name("windows.ps1");
                std::process::Command::new("powershell.exe")
                    .creation_flags(0x08000000)
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(script)
                    .args(["-Action", "disable"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
            }
            Ok(())
        }
        _ => {
            respond(
                stream,
                "404 Not Found",
                "text/plain",
                "Unknown management operation\n",
            )
            .await
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guard_requires_local_host_and_explicit_write_header() {
        let address = "127.0.0.1:18080".parse().unwrap();
        assert!(!local_host("evil.example:18080", address));
        let mut h = HashMap::new();
        assert!(!write_allowed(&h, address));
        h.insert("x-github-turbo-request".into(), "1".into());
        assert!(write_allowed(&h, address));
        h.insert("origin".into(), "https://evil.example".into());
        assert!(!write_allowed(&h, address));
    }
}
