// SPDX-FileCopyrightText: 2025 Sven Shi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::api::{RulesListResponse, register_api};
use super::config::DynamicDomainSetConfig;
use super::rules::{DynamicDomainMutation, DynamicDomainRuleKind, canonicalize_rules};
use super::storage::{append_rule_file, read_rule_file, rewrite_rule_file};
use crate::core::app_clock::AppClock;
use crate::core::error::{DnsError, Result as DnsResult};
use crate::core::rule_matcher::DomainRuleMatcher;
use crate::proto::{Name, Question};

/// Immutable state published to matchers.
///
/// The snapshot is swapped as one `Arc`, so readers always see a fully compiled
/// matcher and never observe partial file writes or partially rebuilt rule
/// structures.
#[derive(Debug, Default)]
pub(super) struct DynamicDomainSetSnapshot {
    pub(super) matcher: DomainRuleMatcher,
}

/// Ordered canonical rule list plus a set for fast duplicate suppression.
///
/// This mutex is intentionally not touched by `contains_name`; it is only used
/// by writer/API paths where preserving file order and exact rule text matters.
#[derive(Debug, Default)]
struct RuleState {
    rules: Vec<String>,
    known: HashSet<String>,
}

type MutationReply = oneshot::Sender<DnsResult<DynamicDomainMutation>>;

/// All file and snapshot mutations are serialized through one worker.
///
/// Append can be fire-and-forget for learned domains or request/reply for API
/// and synchronous learning. Remove, clear, and reload always wait because they
/// replace the authoritative file contents and must report completion.
#[derive(Debug)]
enum WorkerCommand {
    Append {
        rules: Vec<String>,
        wait: Option<MutationReply>,
    },
    Remove {
        rules: Vec<String>,
        wait: MutationReply,
    },
    Clear {
        wait: MutationReply,
    },
    Reload {
        wait: MutationReply,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// Append batch item kept in memory until `batch_size` or `flush_interval_ms`.
#[derive(Debug)]
struct PendingAppend {
    rules: Vec<String>,
    wait: Option<MutationReply>,
}

/// Shared backend for the provider instance.
///
/// It owns both the hot snapshot and the side-effect machinery. The provider
/// object itself is small and mostly delegates here so the API handlers and the
/// executor downcast path can share the same state safely.
#[derive(Debug)]
pub(super) struct DynamicDomainSetBackend {
    tag: String,
    config: DynamicDomainSetConfig,
    /// Canonical source of truth for ordered rules and duplicate checks.
    state: Mutex<RuleState>,
    /// Lock-free read side for matcher hot paths.
    snapshot: ArcSwap<DynamicDomainSetSnapshot>,
    /// Sender becomes available after `init`; stored so API/executor calls can
    /// enqueue work without owning the worker directly.
    tx: Mutex<Option<mpsc::Sender<WorkerCommand>>>,
    /// Joined during plugin destroy to flush pending appends before shutdown.
    worker_handle: Mutex<Option<JoinHandle<()>>>,
}

impl DynamicDomainSetBackend {
    pub(super) fn new(tag: String, config: DynamicDomainSetConfig) -> Self {
        Self {
            tag,
            config,
            state: Mutex::new(RuleState::default()),
            snapshot: ArcSwap::from_pointee(DynamicDomainSetSnapshot::default()),
            tx: Mutex::new(None),
            worker_handle: Mutex::new(None),
        }
    }

    pub(super) fn tag(&self) -> &str {
        &self.tag
    }

    pub(super) async fn start(self: &Arc<Self>) -> DnsResult<()> {
        // Startup is the only place that applies bootstrap rules. After this
        // point the file itself is authoritative, including external edits that
        // become visible through explicit provider reload.
        self.bootstrap_file_if_needed()?;
        let rules = read_rule_file(&self.config.path)?;
        self.install_rules(rules)?;
        let (tx, rx) = mpsc::channel(self.config.queue_size);
        {
            let mut slot = self
                .tx
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set sender lock poisoned"))?;
            *slot = Some(tx);
        }
        let backend = self.clone();
        let handle = tokio::spawn(async move {
            backend.run_worker(rx).await;
        });
        {
            let mut slot = self
                .worker_handle
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set worker lock poisoned"))?;
            *slot = Some(handle);
        }
        register_api(self)?;
        Ok(())
    }

    pub(super) async fn shutdown(&self) -> DnsResult<()> {
        // Ask the worker to drain pending append batches before the runtime
        // drops it. If the channel is already closed there is nothing left to
        // flush from this backend.
        let tx = self.sender()?;
        let (done_tx, done_rx) = oneshot::channel();
        if tx
            .send(WorkerCommand::Shutdown { done: done_tx })
            .await
            .is_ok()
        {
            let _ = done_rx.await;
        }
        let handle = self
            .worker_handle
            .lock()
            .map_err(|_| DnsError::runtime("dynamic_domain_set worker lock poisoned"))?
            .take();
        if let Some(handle) = handle {
            match handle.await {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    return Err(DnsError::runtime(format!(
                        "dynamic_domain_set worker failed: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    #[inline]
    pub(super) fn contains_name(&self, name: &Name) -> bool {
        // Hot path: one atomic snapshot load plus matcher lookup. No locks, no
        // filesystem access, and no rule parsing happen per request.
        self.snapshot.load().matcher.is_match_name(name)
    }

    #[inline]
    pub(super) fn contains_question(&self, question: &Question) -> bool {
        self.contains_name(question.name())
    }

    pub(super) async fn reload(&self) -> DnsResult<()> {
        self.reload_sync().await.map(|_| ())
    }

    pub(crate) fn append_rules_async(
        &self,
        raw_rules: Vec<String>,
        default_kind: DynamicDomainRuleKind,
    ) -> DnsResult<DynamicDomainMutation> {
        let rules = canonicalize_rules(raw_rules, default_kind, "append")?;
        // Stage before enqueue so repeated DNS queries are deduplicated even
        // while the background worker is still waiting for the next flush tick.
        let staged = self.stage_new_rules(rules)?;
        if staged.rules.is_empty() {
            return Ok(staged.mutation);
        }
        let tx = self.sender()?;
        match tx.try_send(WorkerCommand::Append {
            rules: staged.rules.clone(),
            wait: None,
        }) {
            Ok(()) => Ok(staged.mutation),
            Err(err) => {
                // The caller was told only about accepted staged rules, so a
                // failed enqueue must roll the in-memory index back to keep API
                // list output and future duplicate checks honest.
                self.rollback_staged_rules(&staged.rules);
                Err(DnsError::plugin(format!(
                    "dynamic_domain_set '{}' append queue failed: {}",
                    self.tag, err
                )))
            }
        }
    }

    pub(crate) async fn append_rules_sync(
        &self,
        raw_rules: Vec<String>,
        default_kind: DynamicDomainRuleKind,
        timeout_duration: Duration,
    ) -> DnsResult<DynamicDomainMutation> {
        let rules = canonicalize_rules(raw_rules, default_kind, "append")?;
        let staged = self.stage_new_rules(rules)?;
        if staged.rules.is_empty() {
            return Ok(staged.mutation);
        }
        // Synchronous callers use the same worker path as async learning. That
        // keeps ordering with remove/clear/reload identical while giving API
        // handlers a durable "written and snapshot-swapped" acknowledgement.
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.sender()?;
        let send_result = tokio::time::timeout(
            timeout_duration,
            tx.send(WorkerCommand::Append {
                rules: staged.rules.clone(),
                wait: Some(reply_tx),
            }),
        )
        .await;
        match send_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                self.rollback_staged_rules(&staged.rules);
                return Err(DnsError::plugin(format!(
                    "dynamic_domain_set '{}' append queue closed: {}",
                    self.tag, err
                )));
            }
            Err(_) => {
                self.rollback_staged_rules(&staged.rules);
                return Err(DnsError::plugin(format!(
                    "dynamic_domain_set '{}' append timed out enqueueing work",
                    self.tag
                )));
            }
        }
        tokio::time::timeout(timeout_duration, reply_rx)
            .await
            .map_err(|_| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' append timed out waiting for flush",
                    self.tag
                ))
            })?
            .map_err(|_| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' append worker dropped reply",
                    self.tag
                ))
            })?
    }

    pub(super) async fn remove_rules_sync(
        &self,
        raw_rules: Vec<String>,
        default_kind: DynamicDomainRuleKind,
    ) -> DnsResult<DynamicDomainMutation> {
        let rules = canonicalize_rules(raw_rules, default_kind, "remove")?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender()?
            .send(WorkerCommand::Remove {
                rules,
                wait: reply_tx,
            })
            .await
            .map_err(|err| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' remove queue closed: {}",
                    self.tag, err
                ))
            })?;
        reply_rx.await.map_err(|_| {
            DnsError::plugin(format!(
                "dynamic_domain_set '{}' remove worker dropped reply",
                self.tag
            ))
        })?
    }

    pub(super) async fn clear_sync(&self) -> DnsResult<DynamicDomainMutation> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender()?
            .send(WorkerCommand::Clear { wait: reply_tx })
            .await
            .map_err(|err| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' clear queue closed: {}",
                    self.tag, err
                ))
            })?;
        reply_rx.await.map_err(|_| {
            DnsError::plugin(format!(
                "dynamic_domain_set '{}' clear worker dropped reply",
                self.tag
            ))
        })?
    }

    pub(super) async fn reload_sync(&self) -> DnsResult<DynamicDomainMutation> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender()?
            .send(WorkerCommand::Reload { wait: reply_tx })
            .await
            .map_err(|err| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' reload queue closed: {}",
                    self.tag, err
                ))
            })?;
        reply_rx.await.map_err(|_| {
            DnsError::plugin(format!(
                "dynamic_domain_set '{}' reload worker dropped reply",
                self.tag
            ))
        })?
    }

    pub(super) fn list_rules(&self, cursor: usize, limit: usize) -> DnsResult<RulesListResponse> {
        let state = self
            .state
            .lock()
            .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?;
        let total = state.rules.len();
        let start = cursor.min(total);
        let end = start.saturating_add(limit).min(total);
        let rules = state.rules[start..end].to_vec();
        let next_cursor = (end < total).then_some(end);
        Ok(RulesListResponse::new(total, next_cursor, rules))
    }

    #[cfg(test)]
    pub(super) fn store_snapshot_for_test(&self, snapshot: DynamicDomainSetSnapshot) {
        self.snapshot.store(Arc::new(snapshot));
    }

    fn sender(&self) -> DnsResult<mpsc::Sender<WorkerCommand>> {
        self.tx
            .lock()
            .map_err(|_| DnsError::runtime("dynamic_domain_set sender lock poisoned"))?
            .clone()
            .ok_or_else(|| {
                DnsError::plugin(format!(
                    "dynamic_domain_set '{}' worker is not initialized",
                    self.tag
                ))
            })
    }

    fn bootstrap_file_if_needed(&self) -> DnsResult<()> {
        if self.config.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.config.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let rules = canonicalize_rules(
            self.config.bootstrap_rules.clone(),
            DynamicDomainRuleKind::Domain,
            "bootstrap_rules",
        )?;
        // Bootstrap writes canonical rules immediately so later API rewrites do
        // not have to preserve a separate "initial rules" concept.
        rewrite_rule_file(&self.config.path, &rules)?;
        Ok(())
    }

    fn install_rules(&self, rules: Vec<String>) -> DnsResult<DynamicDomainMutation> {
        let snapshot = build_snapshot(&rules)?;
        let total = rules.len();
        {
            // State and snapshot are updated in this order so API list output
            // and hot-path matching converge on the same rule set immediately
            // after the snapshot swap.
            let mut state = self
                .state
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?;
            state.known = rules.iter().cloned().collect();
            state.rules = rules;
        }
        self.snapshot.store(Arc::new(snapshot));
        Ok(DynamicDomainMutation {
            added: 0,
            removed: 0,
            total,
        })
    }

    fn stage_new_rules(&self, rules: Vec<String>) -> DnsResult<StagedRules> {
        let mut staged = Vec::new();
        let total = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?;
            for rule in rules {
                // Insert into both structures while holding one lock so the
                // ordered list and duplicate set cannot drift apart.
                if state.known.insert(rule.clone()) {
                    state.rules.push(rule.clone());
                    staged.push(rule);
                }
            }
            state.rules.len()
        };
        Ok(StagedRules {
            mutation: DynamicDomainMutation {
                added: staged.len(),
                removed: 0,
                total,
            },
            rules: staged,
        })
    }

    fn rollback_staged_rules(&self, rules: &[String]) {
        if rules.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            for rule in rules {
                state.known.remove(rule);
            }
            state.rules.retain(|rule| !rules.iter().any(|v| v == rule));
        }
    }

    fn flush_appends(&self, pending: &mut Vec<PendingAppend>) {
        if pending.is_empty() {
            return;
        }
        // All pending append batches are physically appended in one lock scope,
        // then a fresh matcher snapshot is compiled from the in-memory rule
        // list. That gives learned domains near-real-time matching without a
        // full plugin reload.
        let appended_rules = pending
            .iter()
            .flat_map(|item| item.rules.iter().cloned())
            .collect::<Vec<_>>();
        let result = append_rule_file(&self.config.path, &appended_rules)
            .and_then(|_| self.rebuild_snapshot_from_state());
        match result {
            Ok(total) => {
                info!(
                    plugin = %self.tag,
                    added = appended_rules.len(),
                    total,
                    "dynamic_domain_set appended rules"
                );
                for item in pending.drain(..) {
                    if let Some(wait) = item.wait {
                        let _ = wait.send(Ok(DynamicDomainMutation {
                            added: item.rules.len(),
                            removed: 0,
                            total,
                        }));
                    }
                }
            }
            Err(err) => {
                warn!(
                    plugin = %self.tag,
                    added = appended_rules.len(),
                    error = %err,
                    "dynamic_domain_set append flush failed"
                );
                // Flush failure means the file and snapshot were not advanced.
                // Remove staged rules so later retries can enqueue them again.
                self.rollback_staged_rules(&appended_rules);
                let message = err.to_string();
                for item in pending.drain(..) {
                    if let Some(wait) = item.wait {
                        let _ = wait.send(Err(DnsError::plugin(message.clone())));
                    }
                }
            }
        }
    }

    fn remove_rules(&self, rules: Vec<String>) -> DnsResult<DynamicDomainMutation> {
        let (removed, total, current_rules) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?;
            let before = state.rules.len();
            for rule in &rules {
                state.known.remove(rule);
            }
            state.rules.retain(|rule| !rules.iter().any(|v| v == rule));
            let removed = before.saturating_sub(state.rules.len());
            (removed, state.rules.len(), state.rules.clone())
        };
        // Deletes rewrite the machine-managed file so removed rules cannot
        // reappear on the next provider reload.
        rewrite_rule_file(&self.config.path, &current_rules)?;
        self.rebuild_snapshot_from_rules(&current_rules)?;
        Ok(DynamicDomainMutation {
            added: 0,
            removed,
            total,
        })
    }

    fn clear_rules(&self) -> DnsResult<DynamicDomainMutation> {
        let removed = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?;
            let removed = state.rules.len();
            state.rules.clear();
            state.known.clear();
            removed
        };
        rewrite_rule_file(&self.config.path, &[])?;
        self.rebuild_snapshot_from_rules(&[])?;
        Ok(DynamicDomainMutation {
            added: 0,
            removed,
            total: 0,
        })
    }

    fn reload_from_file(&self) -> DnsResult<DynamicDomainMutation> {
        let rules = read_rule_file(&self.config.path)?;
        let total = rules.len();
        self.install_rules(rules)?;
        Ok(DynamicDomainMutation {
            added: 0,
            removed: 0,
            total,
        })
    }

    fn rebuild_snapshot_from_state(&self) -> DnsResult<usize> {
        let rules = self
            .state
            .lock()
            .map_err(|_| DnsError::runtime("dynamic_domain_set state lock poisoned"))?
            .rules
            .clone();
        self.rebuild_snapshot_from_rules(&rules)?;
        Ok(rules.len())
    }

    fn rebuild_snapshot_from_rules(&self, rules: &[String]) -> DnsResult<()> {
        let snapshot = build_snapshot(rules)?;
        self.snapshot.store(Arc::new(snapshot));
        Ok(())
    }

    async fn run_worker(self: Arc<Self>, mut rx: mpsc::Receiver<WorkerCommand>) {
        // The worker is the only task allowed to touch the rule file. This
        // keeps ordering simple: every mutating API call either waits behind
        // earlier appends or observes their flushed state before it runs.
        let mut pending = Vec::new();
        let mut interval =
            tokio::time::interval(Duration::from_millis(self.config.flush_interval_ms.max(1)));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.flush_appends(&mut pending);
                }
                command = rx.recv() => {
                    let Some(command) = command else {
                        self.flush_appends(&mut pending);
                        break;
                    };
                    match command {
                        WorkerCommand::Append { rules, wait } => {
                            pending.push(PendingAppend { rules, wait });
                            let pending_count: usize = pending.iter().map(|item| item.rules.len()).sum();
                            if pending_count >= self.config.batch_size {
                                self.flush_appends(&mut pending);
                            }
                        }
                        WorkerCommand::Remove { rules, wait } => {
                            // Full-file mutations must see all earlier appends
                            // first, otherwise a pending learned rule could be
                            // appended after a delete/clear/reload reordered it.
                            self.flush_appends(&mut pending);
                            let _ = wait.send(self.remove_rules(rules));
                        }
                        WorkerCommand::Clear { wait } => {
                            self.flush_appends(&mut pending);
                            let _ = wait.send(self.clear_rules());
                        }
                        WorkerCommand::Reload { wait } => {
                            self.flush_appends(&mut pending);
                            let _ = wait.send(self.reload_from_file());
                        }
                        WorkerCommand::Shutdown { done } => {
                            self.flush_appends(&mut pending);
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Rules accepted into memory but not necessarily flushed to disk yet.
#[derive(Debug)]
struct StagedRules {
    mutation: DynamicDomainMutation,
    rules: Vec<String>,
}

pub(super) fn build_snapshot(rules: &[String]) -> DnsResult<DynamicDomainSetSnapshot> {
    let start_ms = AppClock::elapsed_millis();
    let mut matcher = DomainRuleMatcher::default();
    for (idx, rule) in rules.iter().enumerate() {
        matcher
            .add_expression(rule, &format!("dynamic_domain_set.rules[{idx}]"))
            .map_err(DnsError::plugin)?;
    }
    matcher.finalize().map_err(DnsError::plugin)?;
    let elapsed_ms = AppClock::elapsed_millis().saturating_sub(start_ms);
    info!(
        rules = rules.len(),
        full_rules = matcher.full_rule_count(),
        domain_rules = matcher.trie_rule_count(),
        keyword_rules = matcher.keyword_rule_count(),
        regex_rules = matcher.regexp_rule_count(),
        elapsed_ms,
        "dynamic_domain_set snapshot built"
    );
    Ok(DynamicDomainSetSnapshot { matcher })
}
