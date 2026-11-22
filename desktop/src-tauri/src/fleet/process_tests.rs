//! Real subprocess/store/network fixtures. No Copilot account, model prompt, or private task
//! is used: only this test executable can run the deterministic marker-writing executor.
use super::*;
use std::{
    io::Write,
    process::{Child, Command, Stdio},
};

struct Worker {
    child: Child,
}
impl Worker {
    fn start(root: &Path) -> Self {
        let log = std::fs::File::create(root.join("fixture.log")).unwrap();
        Self {
            child: Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fleet::process_tests::fixture_process",
                    "--nocapture",
                ])
                .env("DARBOT_FLEET_TEST_PROCESS_ROOT", root)
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        }
    }
    fn stop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[tokio::test]
async fn fixture_process() {
    let Some(root) = std::env::var_os("DARBOT_FLEET_TEST_PROCESS_ROOT").map(PathBuf::from) else {
        return;
    };
    let node = Node::open(&root).unwrap();
    transport::start(node.clone()).await.unwrap();
    loop {
        let pending: Vec<_> = {
            let r = node.state.lock().unwrap();
            r.tasks
                .values()
                .filter(|t| t.executor == r.node_id && t.status == Status::Queued)
                .map(|t| t.id.clone())
                .collect()
        };
        for task_id in pending {
            node.transaction(|r| {
                let mut task = r.tasks[&task_id].clone();
                task.status = Status::Running;
                task.prompt_dispatched = true;
                task.revision += 1;
                r.publish(task)
            })
            .unwrap();
            let marker = root.join(format!("{task_id}.executed"));
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(marker)
                .unwrap();
            file.write_all(b"known fixture side effect\n").unwrap();
            file.sync_all().unwrap();
            let interrupted =
                node.state.lock().unwrap().tasks[&task_id].prompt == "interrupt-fixture";
            if !interrupted {
                node.transaction(|r| {
                    let mut task = r.tasks[&task_id].clone();
                    task.status = Status::Completed;
                    task.output = "known fixture side effect\n".into();
                    task.revision += 1;
                    task.detail = "Deterministic subprocess fixture completed".into();
                    r.publish(task)
                })
                .unwrap();
                node.message(
                    &task_id,
                    "executor-note".into(),
                    "Explicit fixture result note".into(),
                )
                .unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Fleet subprocess fixture timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn stopped(node: &Arc<Node>) {
    node.shutdown();
    wait_for(|| Arc::strong_count(node) == 1).await;
}
fn terminal(node: &Node, task: &str, status: Status) -> bool {
    node.state
        .lock()
        .unwrap()
        .tasks
        .get(task)
        .is_some_and(|t| t.status == status)
}

#[tokio::test]
async fn three_processes_reconnect_ack_messages_round_robin_and_quarantine_crashed_execution() {
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("fleet-process-{}", id()));
    std::fs::create_dir_all(&root).unwrap();
    let coordinator = Node::open(&root.join("coordinator")).unwrap();
    coordinator
        .transaction(|r| {
            r.enabled = true;
            Ok(())
        })
        .unwrap();
    transport::start(coordinator.clone()).await.unwrap();
    let mut cards = vec![];
    for label in ["b", "c"] {
        let worker = Node::open(&root.join(label)).unwrap();
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        worker
            .transaction(|r| {
                r.enabled = true;
                r.listen = reserved.local_addr().unwrap().to_string();
                r.profiles = vec![Profile {
                    id: "fixture".into(),
                    agent: String::new(),
                    model: "fixture-only".into(),
                    root: root.to_string_lossy().into_owned(),
                    permission_profile: "ask-every-tool".into(),
                }];
                Ok(())
            })
            .unwrap();
        worker
            .pair(coordinator.state.lock().unwrap().card())
            .unwrap();
        let card = worker.state.lock().unwrap().card();
        coordinator.pair(card.clone()).unwrap();
        cards.push(card);
    }
    let mut b = Worker::start(&root.join("b"));
    let c = Worker::start(&root.join("c"));
    wait_for(|| coordinator.presence.lock().unwrap().len() == 2).await;
    let targets: Vec<_> = cards
        .iter()
        .map(|c| Target {
            node_id: c.node_id.clone(),
            profile_id: "fixture".into(),
        })
        .collect();
    let submission = |prompt: &str, targets: Vec<Target>| Submission {
        request_id: id(),
        prompt: prompt.into(),
        targets,
        expires_at: now() + 120_000,
    };
    let first = coordinator
        .submit(submission("complete-fixture-b", targets.clone()))
        .unwrap();
    assert_eq!(first.executor, cards[0].node_id);
    let second = coordinator
        .submit(submission("complete-fixture-c", targets.clone()))
        .unwrap();
    assert_eq!(second.executor, cards[1].node_id);
    wait_for(|| {
        terminal(&coordinator, &first.id, Status::Completed)
            && terminal(&coordinator, &second.id, Status::Completed)
    })
    .await;
    wait_for(|| {
        coordinator.state.lock().unwrap().tasks[&first.id]
            .messages
            .len()
            == 1
    })
    .await;
    coordinator
        .message(
            &first.id,
            "origin-note".into(),
            "Explicit reply note".into(),
        )
        .unwrap();
    wait_for(|| {
        coordinator
            .state
            .lock()
            .unwrap()
            .peers
            .values()
            .all(|p| p.outbox.is_empty())
    })
    .await;
    // The cursor, IDs, ACK watermarks and notes must survive the coordinator process/store lifecycle.
    stopped(&coordinator).await;
    drop(coordinator);
    let coordinator = Node::open(&root.join("coordinator")).unwrap();
    assert_eq!(
        coordinator.state.lock().unwrap().tasks[&first.id].output,
        "known fixture side effect\n"
    );
    assert_eq!(
        coordinator.state.lock().unwrap().tasks[&first.id]
            .messages
            .len(),
        2
    );
    transport::start(coordinator.clone()).await.unwrap();
    wait_for(|| coordinator.presence.lock().unwrap().len() == 2).await;
    let interrupted = coordinator
        .submit(submission("interrupt-fixture", vec![targets[0].clone()]))
        .unwrap();
    wait_for(|| terminal(&coordinator, &interrupted.id, Status::Running)).await;
    b.stop();
    b = Worker::start(&root.join("b"));
    wait_for(|| {
        terminal(
            &coordinator,
            &interrupted.id,
            Status::ReconciliationRequired,
        )
    })
    .await;
    assert!(root
        .join("b")
        .join(format!("{}.executed", interrupted.id))
        .exists());
    assert_eq!(
        std::fs::read_dir(root.join("b"))
            .unwrap()
            .filter_map(|v| v.ok())
            .filter(|v| v.path().extension().is_some_and(|s| s == "executed"))
            .count(),
        2
    );
    assert!(b.child.try_wait().unwrap().is_none());
    stopped(&coordinator).await;
    drop((coordinator, b, c));
    std::fs::remove_dir_all(root).unwrap();
}
