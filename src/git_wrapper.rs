//! `github-turbo git <args>`: run one git command through GitHubTurbo without
//! enabling system integration.
//!
//! The idea comes from fgit: a drop-in `git` replacement that needs no
//! administrator rights and changes no lasting setting. Unlike fgit it uses no
//! third-party mirror or relay, and it never writes git configuration (so there
//! is nothing to restore after a crash or Ctrl+C). A private engine listens on a
//! random loopback port in explicit-proxy mode, and git is started with a
//! per-invocation `-c http.https://github.com/.proxy=...`, which applies to
//! github.com only and is inherited by the git processes it spawns. The engine
//! relays the TLS bytes unchanged to the best GitHub address it finds, so git
//! still verifies GitHub's own certificate and credentials stay end-to-end
//! encrypted. Push and private repositories therefore work as usual.
use super::*;

/// The configuration key for one invocation. Only github.com URLs match it.
pub fn proxy_setting(listen: SocketAddr) -> String {
    format!("http.https://github.com/.proxy=http://{listen}")
}

pub fn git_args(listen: SocketAddr, user: &[String]) -> Vec<String> {
    let mut args = vec!["-c".to_owned(), proxy_setting(listen)];
    args.extend(user.iter().cloned());
    args
}

/// True when a system-wide GitHubTurbo is already accelerating github.com, in
/// which case git needs nothing extra.
async fn system_engine(listen: SocketAddr) -> bool {
    let check = async {
        let mut stream = TcpStream::connect(listen).await.ok()?;
        let request =
            format!("GET /status HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.ok()?;
        let text = String::from_utf8_lossy(&body);
        let json = text.split_once("\r\n\r\n")?.1;
        let status: serde_json::Value = serde_json::from_str(json).ok()?;
        (status["name"] == "GitHubTurbo" && status["transparent"] == true).then_some(())
    };
    timeout(Duration::from_millis(500), check)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn run_git(args: Vec<String>) -> Result<i32> {
    let status =
        tokio::task::spawn_blocking(move || std::process::Command::new("git").args(args).status())
            .await?
            .map_err(|e| err(&format!("cannot start git: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

pub async fn run(mut config: Config, user: &[String]) -> Result<i32> {
    // Console Ctrl+C reaches git too. Let git decide how to stop and report its
    // exit code, instead of the engine vanishing under it.
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
    if config.transparent.enabled && system_engine(config.listen).await {
        return run_git(user.to_vec()).await;
    }
    // Engine notes would mix into git's output; GITHUB_TURBO_DEBUG shows them.
    QUIET.store(
        std::env::var_os("GITHUB_TURBO_DEBUG").is_none(),
        Ordering::Relaxed,
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    config.listen = listener.local_addr()?;
    config.transparent.enabled = false;
    // An upstream on the chosen port would loop; validate() rejects that.
    let state = State::new(config)?;
    state.restore_routes().await;
    state.sources.restore(&state.config.transparent.state_dir);
    // Choose the github.com route while git is still starting.
    let early = state.clone();
    tokio::spawn(async move {
        let _ = early.select("github.com").await;
    });
    let slots = Arc::new(Semaphore::new(state.config.max_connections));
    let serving = state.clone();
    let server = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                incoming = listener.accept() => {
                    let Ok((stream, _)) = incoming else { continue };
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let state = serving.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = handle(stream, state).await {
                            note!("github-turbo: {error}");
                        }
                    });
                }
            }
        }
    });
    let code = run_git(git_args(state.config.listen, user)).await;
    server.abort();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_applies_to_github_only_and_precedes_the_subcommand() {
        let listen: SocketAddr = "127.0.0.1:50123".parse().unwrap();
        let args = git_args(listen, &["clone".into(), "https://github.com/a/b".into()]);
        assert_eq!(
            args,
            [
                "-c",
                "http.https://github.com/.proxy=http://127.0.0.1:50123",
                "clone",
                "https://github.com/a/b"
            ]
        );
    }

    #[tokio::test]
    async fn no_system_engine_on_a_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap();
        drop(listener);
        assert!(!system_engine(port).await);
    }

    #[tokio::test]
    async fn a_running_explicit_engine_is_not_mistaken_for_system_integration() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await;
            let body = r#"{"name":"GitHubTurbo","transparent":false}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(reply.as_bytes()).await.unwrap();
        });
        assert!(!system_engine(listen).await);
    }
}
