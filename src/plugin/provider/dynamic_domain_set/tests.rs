// SPDX-FileCopyrightText: 2025 Sven Shi
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::Method;
use http_body_util::BodyExt;

use super::DynamicDomainSet;
use super::api::{RulesAddHandler, RulesClearHandler, RulesRemoveHandler};
use super::backend::{DynamicDomainSetBackend, DynamicDomainSetSnapshot};
use super::config::DynamicDomainSetConfig;
use super::rules::{DynamicDomainRuleKind, canonicalize_rule};
use super::storage::read_rule_file;
use crate::api::ApiHandler;
use crate::core::app_clock::AppClock;
use crate::core::rule_matcher::DomainRuleMatcher;
use crate::plugin::provider::Provider;
use crate::proto::{DNSClass, Message, Name, Question, RecordType};

fn test_name(raw: &str) -> Name {
    Name::from_ascii(raw).expect("name should parse")
}

fn test_question(raw: &str) -> Question {
    Question::new(test_name(raw), RecordType::A, DNSClass::IN)
}

fn test_config(path: PathBuf) -> DynamicDomainSetConfig {
    DynamicDomainSetConfig {
        path,
        bootstrap_rules: Vec::new(),
        queue_size: 8,
        batch_size: 1,
        flush_interval_ms: 10,
    }
}

#[test]
fn canonicalize_rule_normalizes_plain_full_domain() {
    let rule = canonicalize_rule(" WWW.Example.COM. ", DynamicDomainRuleKind::Full, "test")
        .expect("rule should canonicalize");
    assert_eq!(rule, "full:www.example.com");
}

#[test]
fn read_file_ignores_empty_comments_and_deduplicates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rules.txt");
    fs::write(
        &path,
        "\n# comment\nExample.COM\nfull:WWW.Example.COM.\nexample.com\n",
    )
    .expect("write rules");
    let rules = read_rule_file(&path).expect("rules should load");
    assert_eq!(rules, vec!["domain:example.com", "full:www.example.com"]);
}

#[tokio::test]
async fn dynamic_domain_set_appends_and_matches() {
    AppClock::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("learned.txt");
    let backend = Arc::new(DynamicDomainSetBackend::new(
        "learned".to_string(),
        test_config(path.clone()),
    ));
    backend.start().await.expect("backend should start");
    backend
        .append_rules_sync(
            vec!["Example.COM.".to_string()],
            DynamicDomainRuleKind::Full,
            Duration::from_secs(2),
        )
        .await
        .expect("append should succeed");

    assert!(backend.contains_name(&test_name("example.com.")));
    assert_eq!(
        fs::read_to_string(&path).expect("file should exist"),
        "full:example.com\n"
    );

    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dynamic_domain_set_remove_clear_and_reload() {
    AppClock::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("learned.txt");
    fs::write(&path, "full:one.example\n").expect("write initial");
    let backend = Arc::new(DynamicDomainSetBackend::new(
        "learned".to_string(),
        test_config(path.clone()),
    ));
    backend.start().await.expect("backend should start");
    assert!(backend.contains_name(&test_name("one.example.")));

    backend
        .remove_rules_sync(
            vec!["full:one.example".to_string()],
            DynamicDomainRuleKind::Full,
        )
        .await
        .expect("remove");
    assert!(!backend.contains_name(&test_name("one.example.")));

    fs::write(&path, "full:two.example\n").expect("external edit");
    backend.reload_sync().await.expect("reload");
    assert!(backend.contains_name(&test_name("two.example.")));

    backend.clear_sync().await.expect("clear");
    assert!(!backend.contains_name(&test_name("two.example.")));
    assert_eq!(fs::read_to_string(&path).expect("file"), "");
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dynamic_domain_set_rule_api_adds_removes_and_clears() {
    AppClock::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("learned.txt");
    let backend = Arc::new(DynamicDomainSetBackend::new(
        "learned".to_string(),
        test_config(path.clone()),
    ));
    backend.start().await.expect("backend should start");

    let add = RulesAddHandler {
        backend: backend.clone(),
    };
    let response = add
        .handle(
            http::Request::builder()
                .method(Method::POST)
                .uri("/rules")
                .body(Bytes::from_static(
                    br#"{"rules":["Api.Example."],"rule_kind":"full"}"#,
                ))
                .expect("request"),
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let _ = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert!(backend.contains_name(&test_name("api.example.")));

    let remove = RulesRemoveHandler {
        backend: backend.clone(),
    };
    let response = remove
        .handle(
            http::Request::builder()
                .method(Method::DELETE)
                .uri("/rules")
                .body(Bytes::from_static(br#"{"rules":["full:api.example"]}"#))
                .expect("request"),
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(!backend.contains_name(&test_name("api.example.")));

    backend
        .append_rules_sync(
            vec!["clear.example".to_string()],
            DynamicDomainRuleKind::Full,
            Duration::from_secs(2),
        )
        .await
        .expect("append before clear");
    let clear = RulesClearHandler {
        backend: backend.clone(),
    };
    let response = clear
        .handle(
            http::Request::builder()
                .method(Method::POST)
                .uri("/rules/clear")
                .body(Bytes::new())
                .expect("request"),
        )
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(fs::read_to_string(&path).expect("file"), "");
    assert!(!backend.contains_name(&test_name("clear.example.")));

    backend.shutdown().await.expect("shutdown");
}

#[test]
fn contains_question_uses_name_matching() {
    let mut matcher = DomainRuleMatcher::default();
    matcher
        .add_expression("domain:example.com", "test")
        .expect("rule");
    matcher.finalize().expect("finalize");
    let backend = Arc::new(DynamicDomainSetBackend::new(
        "learned".to_string(),
        test_config(PathBuf::from("unused")),
    ));
    backend.store_snapshot_for_test(DynamicDomainSetSnapshot { matcher });
    let provider = DynamicDomainSet {
        tag: "learned".to_string(),
        backend,
    };
    assert!(provider.contains_question(&test_question("www.example.com.")));

    let request = Message::new();
    let _ctx = crate::core::context::DnsContext::new(
        SocketAddr::new("127.0.0.1".parse().unwrap(), 53),
        request,
    );
}
