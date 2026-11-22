use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const MAX_FRAME: usize = 1024 * 1024;
const UPDATE_ALPN: &[u8] = b"darbot-fleet/1+updates1";
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Exchange {
    version: u32,
    origin: String,
    destination: String,
    ack: u64,
    events: Vec<Envelope>,
    profiles: Vec<Profile>,
    busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    update_hint: Option<crate::desktop_updates::UpdateHint>,
}
pub(super) fn identity(
    r: &Record,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(r.private_key.clone()));
    let chain = vec![CertificateDer::from(r.certificate.clone())];
    rustls::crypto::ring::sign::any_supported_type(&key).map_err(tls)?;
    Ok((chain, key))
}
fn tls(error: impl std::fmt::Display) -> FleetError {
    fail("tls", error.to_string())
}
fn roots(certs: impl Iterator<Item = Vec<u8>>) -> Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(CertificateDer::from(cert)).map_err(tls)?;
    }
    Ok(roots)
}
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
fn server(r: &Record) -> Result<Arc<rustls::ServerConfig>> {
    let trusted = roots(r.peers.values().map(|p| p.card.certificate.clone()))?;
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(trusted), provider())
            .build()
            .map_err(tls)?;
    let (cert, key) = identity(r)?;
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert, key)
        .map_err(tls)?;
    config.alpn_protocols = vec![
        UPDATE_ALPN.to_vec(),
        b"darbot-fleet/1".to_vec(),
        b"http/1.1".to_vec(),
    ];
    Ok(Arc::new(config))
}
fn client(r: &Record, peer: &Peer) -> Result<Arc<rustls::ClientConfig>> {
    let trusted = roots(std::iter::once(peer.card.certificate.clone()))?;
    let (cert, key) = identity(r)?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .with_root_certificates(trusted)
        .with_client_auth_cert(cert, key)
        .map_err(tls)?;
    config.alpn_protocols = vec![UPDATE_ALPN.to_vec(), b"darbot-fleet/1".to_vec()];
    Ok(Arc::new(config))
}
async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Exchange> {
    let length = stream.read_u32().await.map_err(tls)? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(fail("frame_limit", "Fleet frame exceeds limit"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.map_err(tls)?;
    serde_json::from_slice(&bytes).map_err(|e| fail("protocol", e.to_string()))
}
async fn write<S: AsyncWrite + Unpin>(stream: &mut S, exchange: &Exchange) -> Result<()> {
    let bytes = serde_json::to_vec(exchange).map_err(tls)?;
    if bytes.len() > MAX_FRAME {
        return Err(fail("frame_limit", "Fleet response exceeds limit"));
    }
    stream.write_u32(bytes.len() as u32).await.map_err(tls)?;
    stream.write_all(&bytes).await.map_err(tls)?;
    stream.flush().await.map_err(tls)
}
#[cfg(test)]
fn outgoing(node: &Node, peer_id: &str) -> Result<Exchange> {
    outgoing_with_updates(node, peer_id, false)
}
fn outgoing_with_updates(node: &Node, peer_id: &str, updates: bool) -> Result<Exchange> {
    let r = node.state.lock().unwrap();
    if !r.enabled {
        return Err(fail("disabled", "Fleet disabled"));
    }
    let p = r
        .peers
        .get(peer_id)
        .ok_or_else(|| fail("revoked", "Peer revoked"))?;
    let mut exchange = Exchange {
        version: VERSION,
        origin: r.node_id.clone(),
        destination: peer_id.into(),
        ack: p.received,
        events: vec![],
        profiles: r.profiles.clone(),
        busy: r
            .tasks
            .values()
            .any(|t| t.executor == r.node_id && !t.status.terminal()),
        update_hint: if updates {
            r.advertised_updates.get(peer_id).cloned()
        } else {
            None
        },
    };
    for event in p.outbox.iter().take(4) {
        exchange.events.push(event.clone());
        if serde_json::to_vec(&exchange).map_err(tls)?.len() > MAX_FRAME {
            exchange.events.pop();
            if exchange.events.is_empty() {
                return Err(fail(
                    "frame_limit",
                    "Stored event exceeds the transport limit",
                ));
            }
            break;
        }
    }
    Ok(exchange)
}
#[cfg(test)]
fn receive(node: &Node, authenticated_peer: &str, exchange: Exchange) -> Result<()> {
    receive_with_updates(node, authenticated_peer, exchange, false)
}
fn receive_with_updates(
    node: &Node,
    authenticated_peer: &str,
    exchange: Exchange,
    updates: bool,
) -> Result<()> {
    if let Some(hint) = &exchange.update_hint {
        if !updates {
            return Err(fail(
                "protocol",
                "Release hints require negotiated update support",
            ));
        }
        hint.validate()
            .map_err(|_| fail("protocol", "Invalid release hint"))?;
    }
    if exchange.version != VERSION
        || exchange.origin != authenticated_peer
        || exchange.events.len() > 4
        || exchange.profiles.len() > 16
    {
        return Err(fail(
            "protocol",
            "Unsupported version, identity or batch size",
        ));
    }
    for p in &exchange.profiles {
        // The remote root is descriptive only; execution always matches a locally approved profile.
        if p.id.len() > 80
            || p.root.len() > 1024
            || p.model.len() > 128
            || p.agent.len() > 128
            || p.permission_profile != "ask-every-tool"
        {
            return Err(fail("protocol", "Invalid advertised profile"));
        }
    }
    node.transaction(|r| {
        if !r.enabled || exchange.destination != r.node_id {
            return Err(fail("disabled", "Wrong destination or disabled"));
        }
        let peer = r
            .peers
            .get(authenticated_peer)
            .ok_or_else(|| fail("revoked", "Peer revoked"))?;
        if exchange.ack >= peer.next_sequence {
            return Err(fail("protocol", "Acknowledgement exceeds sent sequence"));
        }
        let mut cursor = peer.received;
        if let Some(hint) = exchange.update_hint {
            r.update_hint = Some(hint);
        }
        for envelope in exchange.events {
            if envelope.origin != authenticated_peer
                || envelope.destination != r.node_id
                || envelope.id.len() != 32
            {
                return Err(fail("protocol", "Invalid envelope identity"));
            }
            if envelope.sequence <= cursor {
                continue;
            }
            if envelope.sequence != cursor + 1 {
                return Err(fail("sequence_gap", "Replay from the acknowledged cursor"));
            }
            match envelope.event {
                Event::Submit(mut t) => {
                    if t.origin != authenticated_peer
                        || t.executor != r.node_id
                        || t.id.len() != 32
                        || t.prompt.chars().count() > 2000
                        || t.prompt.lines().count() > 24
                        || t.prompt.trim().is_empty()
                        || t.prompt.trim_start().starts_with('/')
                        || t.status != Status::Queued
                        || t.revision != 0
                        || !t.output.is_empty()
                        || !t.artifacts.is_empty()
                        || t.session_id.is_some()
                        || t.request_id.len() > 100
                        || t.cancel_requested
                        || t.prompt_dispatched
                        || t.a2a
                        || !t.messages.is_empty()
                        || t.expires_at > now() + 24 * 60 * 60 * 1000
                    {
                        return Err(fail("protocol", "Invalid submitted task"));
                    }
                    if !r.tasks.contains_key(&t.id) {
                        if r.tasks.len() >= MAX_TASKS {
                            return Err(fail("queue_full", "Executor task history is full"));
                        }
                        if !r.profiles.contains(&t.profile)
                            || t.expires_at <= now()
                            || r.tasks.values().any(|other| {
                                other.executor == r.node_id && !other.status.terminal()
                            })
                        {
                            t.status = Status::Failed;
                            t.detail =
                                "Profile unavailable, task expired, or executor capacity reserved"
                                    .into();
                            t.revision = 1;
                            r.publish(t)?;
                        } else {
                            r.tasks.insert(t.id.clone(), t);
                        }
                    }
                }
                Event::Update(t) => {
                    let old = r
                        .tasks
                        .get(&t.id)
                        .ok_or_else(|| fail("protocol", "Unknown task update"))?;
                    if old.executor != authenticated_peer
                        || t.executor != old.executor
                        || t.origin != r.node_id
                        || t.profile != old.profile
                        || t.prompt != old.prompt
                        || t.request_id != old.request_id
                        || t.expires_at != old.expires_at
                        || t.output.len() > MAX_OUTPUT
                        || t.detail.len() > 8192
                        || t.artifacts.len() > 1
                        || t.artifacts.iter().any(|a| {
                            a.name.len() > 128
                                || a.media_type != "text/plain"
                                || a.bytes != t.output.len()
                                || a.sha256 != fingerprint(t.output.as_bytes())
                        })
                        || t.session_id.as_ref().is_some_and(|s| s.len() > 512)
                        || t.a2a
                    {
                        return Err(fail(
                            "protocol",
                            "Task update changed authority or exceeded limits",
                        ));
                    }
                    if t.revision > old.revision
                        && (!old.status.terminal()
                            || (old.status == Status::ReconciliationRequired
                                && t.status.terminal()))
                    {
                        let cancelled = old.cancel_requested;
                        let mut t = t;
                        t.cancel_requested |= cancelled;
                        t.messages = old.messages.clone();
                        r.tasks.insert(t.id.clone(), t);
                    }
                }
                Event::Cancel { task_id } => {
                    let t = r
                        .tasks
                        .get_mut(&task_id)
                        .ok_or_else(|| fail("protocol", "Unknown cancellation"))?;
                    if t.origin != authenticated_peer || t.executor != r.node_id {
                        return Err(fail("forbidden", "Cancellation owner mismatch"));
                    }
                    if !t.status.terminal() {
                        t.cancel_requested = true;
                    }
                }
                Event::Message { task_id, message } => {
                    let t = r
                        .tasks
                        .get_mut(&task_id)
                        .ok_or_else(|| fail("protocol", "Unknown message task"))?;
                    if t.a2a
                        || message.author != authenticated_peer
                        || (t.origin != authenticated_peer && t.executor != authenticated_peer)
                        || (t.origin != r.node_id && t.executor != r.node_id)
                        || message.id.is_empty()
                        || message.id.len() > 100
                        || message.text.trim().is_empty()
                        || message.text.chars().count() > 2000
                    {
                        return Err(fail("protocol", "Invalid task message"));
                    }
                    if let Some(old) = t
                        .messages
                        .iter()
                        .find(|m| m.author == message.author && m.id == message.id)
                    {
                        if old.text != message.text {
                            return Err(fail("duplicate", "Message ID conflict"));
                        }
                    } else {
                        if t.messages
                            .iter()
                            .filter(|m| m.author == message.author)
                            .count()
                            >= 16
                        {
                            return Err(fail("queue_full", "Task message limit"));
                        }
                        t.messages.push(message);
                    }
                }
            }
            cursor = envelope.sequence;
        }
        let peer = r.peers.get_mut(authenticated_peer).unwrap();
        peer.received = cursor;
        peer.acknowledged = peer.acknowledged.max(exchange.ack);
        peer.outbox.retain(|e| e.sequence > peer.acknowledged);
        r.flush_updates()?;
        Ok(())
    })?;
    // Never advertise a successful receive until its task/cursor state is durable.
    node.presence.lock().unwrap().insert(
        authenticated_peer.into(),
        Presence {
            seen_at: now(),
            profiles: exchange.profiles,
            busy: exchange.busy,
            error: None,
        },
    );
    node.cancel_revoked_execution();
    Ok(())
}
pub(super) async fn start(node: Arc<Node>) -> Result<()> {
    let address = node.state.lock().unwrap().listen.clone();
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| fail("listen", e.to_string()))?;
    node.transaction(|r| {
        r.listen = listener.local_addr().map_err(io)?.to_string();
        Ok(())
    })?;
    let incoming = node.clone();
    tokio::spawn(async move {
        let slots = Arc::new(tokio::sync::Semaphore::new(8));
        loop {
            let accepted = tokio::select! { _ = incoming.stop.cancelled() => break, result = listener.accept() => result };
            let (stream, _) = match accepted {
                Ok(value) => value,
                Err(e) => {
                    *incoming.failure.lock().unwrap() = Some(fail("listen", e.to_string()));
                    incoming.shutdown();
                    break;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let node = incoming.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let operation = async {
                    let config = server(&node.state.lock().unwrap())?;
                    let mut stream = tokio::time::timeout(
                        Duration::from_secs(8),
                        TlsAcceptor::from(config).accept(stream),
                    )
                    .await
                    .map_err(|_| fail("timeout", "TLS handshake deadline exceeded"))?
                    .map_err(tls)?;
                    let cert = stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .and_then(|c| c.first())
                        .ok_or_else(|| fail("unauthorized", "Client certificate required"))?;
                    let peer_id = fingerprint(cert.as_ref());
                    if !node.state.lock().unwrap().peers.contains_key(&peer_id) {
                        return Err(fail("unauthorized", "Client certificate is not pinned"));
                    }
                    let updates = stream.get_ref().1.alpn_protocol() == Some(UPDATE_ALPN);
                    match stream.get_ref().1.alpn_protocol() {
                        Some(b"http/1.1") | None => {
                            return super::a2a::serve(node.clone(), peer_id, stream).await;
                        }
                        Some(b"darbot-fleet/1") => {}
                        Some(protocol) if protocol == UPDATE_ALPN => {}
                        _ => return Err(fail("protocol", "Unsupported ALPN")),
                    }
                    let exchange = read(&mut stream).await?;
                    receive_with_updates(&node, &peer_id, exchange, updates)?;
                    write(
                        &mut stream,
                        &outgoing_with_updates(&node, &peer_id, updates)?,
                    )
                    .await
                };
                tokio::select! {
                    _ = node.stop.cancelled() => {},
                    result = tokio::time::timeout(Duration::from_secs(65), operation) => {
                        if let Err(error) = result.unwrap_or_else(|_| Err(fail("timeout", "Incoming connection timed out"))) {
                            *node.failure.lock().unwrap() = Some(error);
                        }
                    },
                }
            });
        }
    });
    let outgoing_node = node.clone();
    tokio::spawn(async move {
        let mut workers: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
        loop {
            let peers: Vec<_> = outgoing_node
                .state
                .lock()
                .unwrap()
                .peers
                .keys()
                .cloned()
                .collect();
            workers.retain(|id, worker| {
                if peers.contains(id) && !worker.is_finished() {
                    true
                } else {
                    worker.abort();
                    false
                }
            });
            for peer in peers {
                if workers.contains_key(&peer) {
                    continue;
                }
                let node = outgoing_node.clone();
                let worker_id = peer.clone();
                workers.insert(peer, tokio::spawn(async move {
                    let mut attempts = 0u32;
                    loop {
                        let result = tokio::select! {
                            _ = node.stop.cancelled() => return,
                            result = tokio::time::timeout(Duration::from_secs(8), sync(&node, &worker_id)) =>
                                result.unwrap_or_else(|_| Err(fail("timeout", "Peer did not answer within 8 seconds"))),
                        };
                        let delay = match result {
                            Ok(()) => { attempts = 0; 2000 }
                            Err(e) => {
                                attempts = (attempts + 1).min(5);
                                node.presence.lock().unwrap().entry(worker_id.clone()).or_default().error = Some(e.message);
                                (1u64 << attempts) * 1000 + rand::random::<u16>() as u64 % 1000
                            }
                        };
                        tokio::select! { _ = node.stop.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_millis(delay)) => {} }
                    }
                }));
            }
            tokio::select! { _ = outgoing_node.stop.cancelled() => {
                for (_, worker) in workers { worker.abort(); }
                return;
            }, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
        }
    });
    Ok(())
}
async fn sync(node: &Node, peer_id: &str) -> Result<()> {
    let (endpoint, config) = {
        let record = node.state.lock().unwrap();
        let peer = record
            .peers
            .get(peer_id)
            .ok_or_else(|| fail("revoked", "Peer revoked"))?;
        (peer.card.endpoint.clone(), client(&record, peer)?)
    };
    let tcp = tokio::net::TcpStream::connect(&endpoint)
        .await
        .map_err(tls)?;
    let mut stream = TlsConnector::from(config)
        .connect(ServerName::try_from("darbot-fleet").unwrap(), tcp)
        .await
        .map_err(tls)?;
    let peer = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| fingerprint(c.as_ref()));
    let updates = stream.get_ref().1.alpn_protocol() == Some(UPDATE_ALPN);
    if peer.as_deref() != Some(peer_id)
        || (!updates && stream.get_ref().1.alpn_protocol() != Some(b"darbot-fleet/1"))
    {
        return Err(fail(
            "unauthorized",
            "Peer certificate pin or protocol changed",
        ));
    }

    write(&mut stream, &outgoing_with_updates(node, peer_id, updates)?).await?;
    receive_with_updates(node, peer_id, read(&mut stream).await?, updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn three_real_tls_nodes_deliver_bidirectionally_deduplicate_restart_and_revoke() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("fleet-tls-{}", id()));
        std::fs::create_dir_all(&root).unwrap();
        let a = Node::open(&root.join("a")).unwrap();
        let b = Node::open(&root.join("b")).unwrap();
        let c = Node::open(&root.join("c")).unwrap();
        let profile = Profile {
            id: "local".into(),
            agent: "".into(),
            model: "test-only".into(),
            root: root.to_string_lossy().into_owned(),
            permission_profile: "ask-every-tool".into(),
        };
        for node in [&a, &b, &c] {
            node.transaction(|r| {
                r.enabled = true;
                r.profiles = vec![profile.clone()];
                Ok(())
            })
            .unwrap();
            start(node.clone()).await.unwrap();
        }
        for (left, right) in [(&a, &b), (&b, &a), (&a, &c), (&c, &a), (&b, &c), (&c, &b)] {
            left.pair(right.state.lock().unwrap().card()).unwrap();
        }
        let aid = a.state.lock().unwrap().node_id.clone();
        let bid = b.state.lock().unwrap().node_id.clone();
        let cid = c.state.lock().unwrap().node_id.clone();
        let hint = crate::desktop_updates::UpdateHint {
            version: "0.0.29".into(),
            build_repository: crate::desktop_updates::BUILD_REPOSITORY.into(),
            source_repository: crate::desktop_updates::SOURCE_REPOSITORY.into(),
            workflow: crate::desktop_updates::WORKFLOW.into(),
            run_id: 7,
            artifact_id: 8,
            build_sha: "a".repeat(40),
            source_sha: "b".repeat(40),
        };
        b.transaction(|r| {
            r.advertised_updates.insert(aid.clone(), hint.clone());
            Ok(())
        })
        .unwrap();
        let (endpoint, mut config) = {
            let r = a.state.lock().unwrap();
            let p = &r.peers[&bid];
            (p.card.endpoint.clone(), client(&r, p).unwrap())
        };
        Arc::make_mut(&mut config).alpn_protocols = vec![b"darbot-fleet/1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(endpoint).await.unwrap();
        let mut legacy = TlsConnector::from(config)
            .connect(ServerName::try_from("darbot-fleet").unwrap(), tcp)
            .await
            .unwrap();
        assert_eq!(
            legacy.get_ref().1.alpn_protocol(),
            Some(b"darbot-fleet/1".as_slice())
        );
        write(&mut legacy, &outgoing(&a, &bid).unwrap())
            .await
            .unwrap();
        let exchange = read(&mut legacy).await.unwrap();
        assert!(exchange.update_hint.is_none());
        assert!(serde_json::to_value(&exchange)
            .unwrap()
            .get("updateHint")
            .is_none());
        receive(&a, &bid, exchange).unwrap();
        assert!(b.state.lock().unwrap().peers[&aid].outbox.is_empty());
        drop(legacy);
        sync(&a, &bid).await.unwrap();
        assert_eq!(a.state.lock().unwrap().update_hint, Some(hint));
        sync(&a, &cid).await.unwrap();
        let targets = vec![
            Target {
                node_id: bid.clone(),
                profile_id: "local".into(),
            },
            Target {
                node_id: cid.clone(),
                profile_id: "local".into(),
            },
        ];
        let first = a
            .submit(Submission {
                request_id: id(),
                prompt: "Selected task A".into(),
                targets: targets.clone(),
                expires_at: now() + 60_000,
            })
            .unwrap();
        assert_eq!(first.executor, bid);
        let replay = outgoing(&a, &bid).unwrap();
        sync(&a, &bid).await.unwrap();
        receive(&b, &aid, replay).unwrap();
        assert_eq!(b.state.lock().unwrap().tasks.len(), 1);
        assert_eq!(b.state.lock().unwrap().peers[&aid].received, 1);
        assert!(a.state.lock().unwrap().peers[&bid].outbox.is_empty());
        let second = a
            .submit(Submission {
                request_id: id(),
                prompt: "Selected task B".into(),
                targets,
                expires_at: now() + 60_000,
            })
            .unwrap();
        assert_eq!(second.executor, cid);
        sync(&a, &cid).await.unwrap();
        // A native executor transition, not an echo service: transport must faithfully deliver it.
        b.transaction(|r| {
            let mut task = r.tasks[&first.id].clone();
            task.status = Status::Running;
            task.prompt_dispatched = true;
            task.revision += 1;
            r.publish(task)
        })
        .unwrap();
        sync(&b, &aid).await.unwrap();
        assert_eq!(
            a.state.lock().unwrap().tasks[&first.id].status,
            Status::Running
        );
        b.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(b);
        let restored = Node::open(&root.join("b")).unwrap();
        assert_eq!(
            restored.state.lock().unwrap().tasks[&first.id].status,
            Status::ReconciliationRequired
        );
        start(restored.clone()).await.unwrap();
        sync(&restored, &aid).await.unwrap();
        assert_eq!(
            a.state.lock().unwrap().tasks[&first.id].status,
            Status::ReconciliationRequired
        );
        let reverse = c
            .submit(Submission {
                request_id: id(),
                prompt: "Selected reverse task".into(),
                targets: vec![Target {
                    node_id: aid.clone(),
                    profile_id: "local".into(),
                }],
                expires_at: now() + 60_000,
            })
            .unwrap();
        sync(&c, &aid).await.unwrap();
        assert!(a.state.lock().unwrap().tasks.contains_key(&reverse.id));
        a.revoke(&cid).unwrap();
        assert!(sync(&c, &aid).await.is_err());
        let stranger = Node::open(&root.join("stranger")).unwrap();
        stranger
            .transaction(|r| {
                r.enabled = true;
                Ok(())
            })
            .unwrap();
        stranger.pair(a.state.lock().unwrap().card()).unwrap();
        assert!(sync(&stranger, &aid).await.is_err());
        for node in [&a, &c, &restored, &stranger] {
            node.shutdown();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop((a, c, restored, stranger));
        std::fs::remove_dir_all(root).unwrap();
    }
}
