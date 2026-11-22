use super::*;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("fleet-test-{}", id()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn node(&self, name: &str) -> Arc<Node> {
        Node::open(&self.root.join(name)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn profile(root: &Path) -> Profile {
    Profile {
        id: "review".into(),
        agent: String::new(),
        model: "test-model".into(),
        root: root.to_string_lossy().into_owned(),
        permission_profile: "ask-every-tool".into(),
    }
}
fn enable(node: &Node, profile: Profile) {
    node.transaction(|r| {
        r.enabled = true;
        r.profiles = vec![profile];
        Ok(())
    })
    .unwrap();
}
fn submit(node: &Node, targets: Vec<Target>) -> Task {
    node.submit(Submission {
        request_id: id(),
        prompt: "Review this selected task".into(),
        targets,
        expires_at: now() + 60_000,
    })
    .unwrap()
}
#[test]
fn restart_quarantines_ambiguous_execution_and_store_corruption_is_visible() {
    let f = Fixture::new();
    let node = f.node("one");
    enable(&node, profile(&f.root));
    let target = Target {
        node_id: node.state.lock().unwrap().node_id.clone(),
        profile_id: "review".into(),
    };
    let task = submit(&node, vec![target]);
    node.transaction(|r| {
        r.tasks.get_mut(&task.id).unwrap().status = Status::Running;
        Ok(())
    })
    .unwrap();
    drop(node);
    let restored = f.node("one");
    assert!(restored.state.lock().unwrap().enabled);
    assert_eq!(
        restored.state.lock().unwrap().tasks[&task.id].status,
        Status::ReconciliationRequired
    );
    drop(restored);
    std::fs::write(f.root.join("one").join("fleet-v1.dpapi"), b"broken").unwrap();
    assert!(Node::open(&f.root.join("one")).is_err());
}
#[test]
fn round_robin_is_durable_and_skips_offline_and_busy_nodes() {
    let f = Fixture::new();
    let a = f.node("a");
    let b = f.node("b");
    let c = f.node("c");
    enable(&a, profile(&f.root));
    let bcard = b.state.lock().unwrap().card();
    let ccard = c.state.lock().unwrap().card();
    let bid = bcard.node_id.clone();
    let cid = ccard.node_id.clone();
    a.pair(bcard).unwrap();
    a.pair(ccard).unwrap();
    for peer in [&bid, &cid] {
        a.presence.lock().unwrap().insert(
            peer.clone(),
            Presence {
                seen_at: now(),
                profiles: vec![profile(&f.root)],
                ..Presence::default()
            },
        );
    }

    let targets = vec![
        Target {
            node_id: bid.clone(),
            profile_id: "review".into(),
        },
        Target {
            node_id: cid.clone(),
            profile_id: "review".into(),
        },
    ];
    let first = submit(&a, targets.clone());
    assert_eq!(first.executor, bid);
    let second = submit(&a, targets.clone());
    assert_eq!(second.executor, cid);
    assert!(a
        .submit(Submission {
            request_id: id(),
            prompt: "No capacity".into(),
            targets,
            expires_at: now() + 60_000
        })
        .is_err());
    let cursor = a.state.lock().unwrap().round_robin;
    drop(a);
    let restored = f.node("a");
    assert_eq!(restored.state.lock().unwrap().round_robin, cursor);
    assert!(restored.presence.lock().unwrap().is_empty());
    restored.revoke(&bid).unwrap();
    assert_eq!(
        restored.state.lock().unwrap().tasks[&first.id].status,
        Status::ReconciliationRequired
    );
}

#[test]
fn store_is_exclusive_and_failed_durability_never_changes_live_state() {
    let f = Fixture::new();
    let node = f.node("one");
    assert_eq!(
        Node::open(&f.root.join("one")).err().unwrap().code,
        "store_busy"
    );
    let before = node.state.lock().unwrap().label.clone();
    let pending = f.root.join("one").join("fleet-v1.pending");
    std::fs::create_dir(&pending).unwrap();
    let failed = node.transaction(|r| {
        r.label = "must not commit".into();
        Ok(())
    });
    assert!(failed.is_err());
    assert_eq!(node.state.lock().unwrap().label, before);
    std::fs::remove_dir(pending).unwrap();
    drop(node);
    assert_eq!(f.node("one").state.lock().unwrap().label, before);
}

#[test]
fn immutable_outbox_snapshots_are_bounded_and_notes_are_deduplicated() {
    let f = Fixture::new();
    let origin = f.node("origin");
    let executor = f.node("executor");
    let origin_id = origin.state.lock().unwrap().node_id.clone();
    enable(&executor, profile(&f.root));
    executor.pair(origin.state.lock().unwrap().card()).unwrap();
    let task_id = id();
    executor
        .transaction(|r| {
            let task = Task {
                id: task_id.clone(),
                request_id: id(),
                origin: origin_id.clone(),
                executor: r.node_id.clone(),
                profile: profile(&f.root),
                prompt: "Fixture".into(),
                expires_at: now() + 60_000,
                status: Status::Running,
                revision: 1,
                session_id: Some("fixture-session".into()),
                output: String::new(),
                detail: String::new(),
                artifacts: vec![],
                cancel_requested: false,
                prompt_dispatched: true,
                a2a: false,
                messages: vec![],
            };
            r.publish(task)
        })
        .unwrap();
    for _ in 0..50 {
        executor
            .transaction(|r| {
                let mut task = r.tasks[&task_id].clone();
                task.output.push('x');
                task.revision += 1;
                r.publish(task)
            })
            .unwrap();
    }
    {
        let r = executor.state.lock().unwrap();
        let peer = &r.peers[&origin_id];
        assert_eq!(peer.outbox.len(), 1);
        assert!(matches!(&peer.outbox[0].event,Event::Update(t) if t.output.is_empty()));
    }
    executor
        .transaction(|r| {
            let peer = r.peers.get_mut(&origin_id).unwrap();
            peer.acknowledged = 1;
            peer.outbox.clear();
            r.flush_updates()
        })
        .unwrap();
    {
        let r = executor.state.lock().unwrap();
        assert!(
            matches!(&r.peers[&origin_id].outbox[0].event,Event::Update(t) if t.output.len()==50)
        );
    }
    executor
        .message(&task_id, "fixed-note".into(), "Selected note".into())
        .unwrap();
    executor
        .message(&task_id, "fixed-note".into(), "Selected note".into())
        .unwrap();
    assert_eq!(
        executor.state.lock().unwrap().tasks[&task_id]
            .messages
            .len(),
        1
    );
    assert!(executor
        .message(&task_id, "fixed-note".into(), "Changed note".into())
        .is_err());
}
