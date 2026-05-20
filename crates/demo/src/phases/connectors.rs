//! Stage 11 — Connector Framework.
//!
//! Drives real `connector_framework::Connector` implementations from
//! the `connectors` crate against synthesised fixture data: Google
//! Drive, Jira, Slack, and Microsoft Graph e-mail. Each connector is
//! authenticated, full-synced, incrementally synced, and walked
//! through a webhook subscription + payload round-trip — exactly the
//! state machine `docs/DESIGN.md` §10.2 specifies
//! for production deployments. Counts and timings are accumulated
//! into [`RuntimeState`] / the demo report so the final markdown
//! summary captures the connector-side surface.

use std::time::Instant;

use chrono::{Duration, Utc};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorEvent, ConnectorInstanceId, ConnectorKind,
    SyncMode, SyncState, SyncStatus,
};
use connectors::{
    email::{
        EmailConnector, EmailProvider, GmailMessage, GmailMessagesListPage, GraphMessage,
        GraphMessagesPage,
    },
    google_drive::{
        GoogleDriveChange, GoogleDriveChangeList, GoogleDriveConnector, GoogleDriveFile,
        GoogleDriveFileList,
    },
    jira::{JiraConnector, JiraFields, JiraIssue, JiraSearchResponse},
    slack::{
        SlackChannel, SlackConnector, SlackHistoryResponse, SlackInitialSyncPage, SlackMessage,
        SlackResponseMetadata,
    },
};
use evidence_store::ScopeId;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "connectors";

/// Per-connector metrics rolled up into the stage report.
struct ConnectorMetrics {
    name: &'static str,
    initial_events: u64,
    incremental_events: u64,
    webhook_events: u64,
    subscriptions: u64,
}

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let phase_started = Instant::now();
    let mut phase = PhaseReport::new("Stage 11: Connector Framework");

    let drive = exercise_google_drive(dataset, log, report);
    let jira = exercise_jira(dataset, log, report);
    let slack = exercise_slack(dataset, log, report);
    let email = exercise_email(dataset, log, report);

    let connectors = [&drive, &jira, &slack, &email];

    let mut total_initial = 0u64;
    let mut total_incremental = 0u64;
    let mut total_webhook = 0u64;
    let mut total_subscriptions = 0u64;
    for c in connectors.iter() {
        total_initial += c.initial_events;
        total_incremental += c.incremental_events;
        total_webhook += c.webhook_events;
        total_subscriptions += c.subscriptions;

        phase.stat(
            format!("{}.initial_events", c.name),
            c.initial_events.to_string(),
        );
        phase.stat(
            format!("{}.incremental_events", c.name),
            c.incremental_events.to_string(),
        );
        phase.stat(
            format!("{}.webhook_events", c.name),
            c.webhook_events.to_string(),
        );
        phase.stat(
            format!("{}.subscriptions", c.name),
            c.subscriptions.to_string(),
        );
    }

    let total_emitted = total_initial + total_incremental + total_webhook;

    phase.stat("connectors.exercised", connectors.len().to_string());
    phase.stat("connectors.initial_events", total_initial.to_string());
    phase.stat(
        "connectors.incremental_events",
        total_incremental.to_string(),
    );
    phase.stat("connectors.webhook_events", total_webhook.to_string());
    phase.stat("connectors.subscriptions", total_subscriptions.to_string());
    phase.stat("connectors.events_total", total_emitted.to_string());

    report.count("connectors.exercised", connectors.len() as u64);
    report.count("connectors.events_total", total_emitted);
    report.count("connectors.subscriptions", total_subscriptions);
    report.count("connectors.webhook_events", total_webhook);

    log.record(
        PHASE,
        "all four connectors emit at least one event",
        connectors
            .iter()
            .all(|c| c.initial_events + c.incremental_events + c.webhook_events > 0),
    );
    log.record(
        PHASE,
        "every connector registered at least one webhook subscription",
        connectors.iter().all(|c| c.subscriptions >= 1),
    );

    state.connectors_exercised = connectors.len() as u64;
    state.connector_events_emitted = total_emitted;
    state.connector_webhooks_parsed = total_webhook;
    state.connector_subscriptions = total_subscriptions;

    // Connector sync events are intentionally not appended to the
    // audit log — `audit_service::AuditActionType` is a closed enum
    // covering canonical promotions, exports, agent proposals, member
    // provisioning, key destruction, policy changes, and tenant
    // lifecycle (per the Audit Service contract). Connector
    // exercise metrics are surfaced via `report.count(...)` and the
    // stage stats above without polluting the audit trail's semantic
    // contract.

    phase.timing = phase_started.elapsed();
    report.add_phase(phase);
}

fn exercise_google_drive(
    _dataset: &Dataset,
    log: &mut AssertionLog,
    report: &mut DemoReport,
) -> ConnectorMetrics {
    let scope = ScopeId::new_v4();
    let cfg = ConnectorConfig::new(ConnectorKind::GoogleDrive, AuthKind::OAuth2, scope);
    let instance = ConnectorInstanceId::new_v4();

    let now = Utc::now();
    let initial_pages = vec![
        GoogleDriveFileList {
            files: vec![
                GoogleDriveFile {
                    id: "drive-doc-1".into(),
                    name: "Q3-launch-plan.gdoc".into(),
                    mime_type: "application/vnd.google-apps.document".into(),
                    trashed: false,
                    modified_time: Some(now - Duration::hours(36)),
                    created_time: Some(now - Duration::hours(36)),
                },
                GoogleDriveFile {
                    id: "drive-doc-2".into(),
                    name: "OKR-tracker.gsheet".into(),
                    mime_type: "application/vnd.google-apps.spreadsheet".into(),
                    trashed: false,
                    modified_time: Some(now - Duration::hours(30)),
                    created_time: Some(now - Duration::hours(72)),
                },
            ],
            next_page_token: Some("page-2".into()),
            new_start_page_token: None,
        },
        GoogleDriveFileList {
            files: vec![GoogleDriveFile {
                id: "drive-doc-3".into(),
                name: "Engineering-RFC-114.pdf".into(),
                mime_type: "application/pdf".into(),
                trashed: false,
                modified_time: Some(now - Duration::hours(8)),
                created_time: Some(now - Duration::hours(8)),
            }],
            next_page_token: None,
            new_start_page_token: Some("changes-token-1".into()),
        },
    ];

    let incremental_pages = vec![GoogleDriveChangeList {
        changes: vec![
            GoogleDriveChange {
                file_id: "drive-doc-2".into(),
                kind: "file".into(),
                removed: false,
                file: Some(GoogleDriveFile {
                    id: "drive-doc-2".into(),
                    name: "OKR-tracker.gsheet".into(),
                    mime_type: "application/vnd.google-apps.spreadsheet".into(),
                    trashed: false,
                    modified_time: Some(now - Duration::minutes(30)),
                    created_time: Some(now - Duration::hours(72)),
                }),
                time: Some(now - Duration::minutes(30)),
            },
            GoogleDriveChange {
                file_id: "drive-doc-stale".into(),
                kind: "file".into(),
                removed: true,
                file: None,
                time: Some(now - Duration::minutes(15)),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("changes-token-2".into()),
    }];

    let connector = GoogleDriveConnector::new(instance)
        .with_initial_pages(initial_pages)
        .with_incremental_pages(incremental_pages);

    let token = connector.authenticate(&cfg).expect("drive auth");
    log.record(
        PHASE,
        "google_drive.authenticate returns drive scope",
        token.scope.contains("drive"),
    );

    let initial_started = Instant::now();
    let initial = connector
        .initial_sync(&cfg, &token)
        .expect("drive initial_sync");
    report.add_benchmark(
        "connectors.google_drive.initial_sync",
        initial.events.len() as u64,
        initial_started.elapsed(),
    );
    log.record(
        PHASE,
        "google_drive.initial_sync emits one event per file",
        initial.events.len() == 3,
    );
    log.record(
        PHASE,
        "google_drive.initial_sync seeds new_start_page_token",
        initial.next_cursor.as_deref() == Some("changes-token-1"),
    );

    let mut sync_state = SyncState::new(instance);
    sync_state.cursor.clone_from(&initial.next_cursor);
    sync_state.mode = SyncMode::Incremental;
    sync_state.status = SyncStatus::Succeeded;

    let inc_started = Instant::now();
    let incremental = connector
        .incremental_sync(&cfg, &token, &sync_state)
        .expect("drive incremental_sync");
    report.add_benchmark(
        "connectors.google_drive.incremental_sync",
        incremental.events.len() as u64,
        inc_started.elapsed(),
    );
    let inc_has_delete = incremental
        .events
        .iter()
        .any(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }));
    log.record(
        PHASE,
        "google_drive.incremental_sync surfaces removed change as DocumentDeleted",
        inc_has_delete,
    );

    let subscription = connector
        .subscribe_webhook(&cfg, &token, "https://demo.example/webhooks/drive")
        .expect("drive webhook subscribe");
    log.record(
        PHASE,
        "google_drive.subscribe_webhook bound to instance",
        subscription.connector == instance,
    );

    let webhook_started = Instant::now();
    let push_payload = serde_json::json!({
        "resourceId": "drive-doc-1",
        "resourceState": "permission_change",
        "userId": "user-99",
        "newRole": "writer",
        "occurredAt": Utc::now(),
    });
    let webhook_events = connector
        .handle_webhook_event(&serde_json::to_vec(&push_payload).unwrap())
        .expect("drive webhook decode");
    report.add_benchmark(
        "connectors.google_drive.webhook",
        webhook_events.len() as u64,
        webhook_started.elapsed(),
    );
    let webhook_is_perm = webhook_events
        .iter()
        .any(|e| matches!(e, ConnectorEvent::PermissionChanged { .. }));
    log.record(
        PHASE,
        "google_drive.handle_webhook_event surfaces permission change",
        webhook_is_perm,
    );

    ConnectorMetrics {
        name: "google_drive",
        initial_events: initial.events.len() as u64,
        incremental_events: incremental.events.len() as u64,
        webhook_events: webhook_events.len() as u64,
        subscriptions: 1,
    }
}

fn issue(key: &str, summary: &str, created: chrono::DateTime<Utc>) -> JiraIssue {
    JiraIssue {
        key: key.into(),
        id: key.replace('-', ""),
        fields: JiraFields {
            summary: summary.into(),
            created: Some(created),
            updated: Some(created),
            status: None,
        },
    }
}

fn exercise_jira(
    _dataset: &Dataset,
    log: &mut AssertionLog,
    report: &mut DemoReport,
) -> ConnectorMetrics {
    let scope = ScopeId::new_v4();
    let cfg = ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, scope);
    let instance = ConnectorInstanceId::new_v4();

    let now = Utc::now();

    let initial_pages = vec![JiraSearchResponse {
        issues: vec![
            issue(
                "PROJ-101",
                "Adopt knowledge substrate",
                now - Duration::days(2),
            ),
            issue(
                "PROJ-102",
                "Wire up agent contract",
                now - Duration::days(1),
            ),
            issue("PROJ-103", "Enable export plane", now - Duration::hours(20)),
        ],
        start_at: 0,
        max_results: 50,
        total: 3,
    }];

    let incremental_pages = vec![JiraSearchResponse {
        issues: vec![
            issue(
                "PROJ-102",
                "Wire up agent contract",
                now - Duration::minutes(45),
            ),
            issue(
                "PROJ-104",
                "Audit log retention review",
                now - Duration::minutes(20),
            ),
        ],
        start_at: 0,
        max_results: 50,
        total: 2,
    }];

    let connector = JiraConnector::new(instance)
        .with_initial_pages(initial_pages)
        .with_incremental_pages(incremental_pages);

    let token = connector.authenticate(&cfg).expect("jira auth");
    log.record(
        PHASE,
        "jira.authenticate returns jira-work scope",
        token.scope.contains("jira-work"),
    );

    let initial_started = Instant::now();
    let initial = connector
        .initial_sync(&cfg, &token)
        .expect("jira initial_sync");
    report.add_benchmark(
        "connectors.jira.initial_sync",
        initial.events.len() as u64,
        initial_started.elapsed(),
    );
    log.record(
        PHASE,
        "jira.initial_sync emits one DocumentCreated per issue",
        initial.events.len() == 3
            && initial
                .events
                .iter()
                .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    let mut sync_state = SyncState::new(instance);
    sync_state.cursor.clone_from(&initial.next_cursor);
    sync_state.mode = SyncMode::Incremental;
    sync_state.status = SyncStatus::Succeeded;

    let inc_started = Instant::now();
    let incremental = connector
        .incremental_sync(&cfg, &token, &sync_state)
        .expect("jira incremental_sync");
    report.add_benchmark(
        "connectors.jira.incremental_sync",
        incremental.events.len() as u64,
        inc_started.elapsed(),
    );
    log.record(
        PHASE,
        "jira.incremental_sync emits at least one DocumentUpdated",
        incremental
            .events
            .iter()
            .any(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. })),
    );

    let subscription = connector
        .subscribe_webhook(&cfg, &token, "https://demo.example/webhooks/jira")
        .expect("jira webhook subscribe");
    log.record(
        PHASE,
        "jira.subscribe_webhook returns subscription bound to instance",
        subscription.connector == instance,
    );

    let webhook_started = Instant::now();
    let mut webhook_total: u64 = 0;
    let created_payload = serde_json::json!({
        "webhookEvent": "jira:issue_created",
        "issue": {
            "key": "PROJ-105",
            "id": "PROJ105",
            "fields": {
                "summary": "Verify connector pipeline",
                "created": Utc::now(),
                "updated": Utc::now(),
            }
        },
        "timestamp": Utc::now().timestamp_millis(),
    });
    let evs = connector
        .handle_webhook_event(&serde_json::to_vec(&created_payload).unwrap())
        .expect("jira created webhook");
    webhook_total += evs.len() as u64;
    log.record(
        PHASE,
        "jira.handle_webhook_event(jira:issue_created) yields DocumentCreated",
        evs.iter()
            .any(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    let perm_payload = serde_json::json!({
        "webhookEvent": "permissionscheme_updated",
        "issueKey": "PROJ-101",
        "accountId": "acct-77",
        "new_role": "developers",
    });
    let perm_evs = connector
        .handle_webhook_event(&serde_json::to_vec(&perm_payload).unwrap())
        .expect("jira permission webhook");
    webhook_total += perm_evs.len() as u64;
    log.record(
        PHASE,
        "jira.handle_webhook_event(permissionscheme_updated) yields PermissionChanged",
        perm_evs
            .iter()
            .any(|e| matches!(e, ConnectorEvent::PermissionChanged { .. })),
    );
    report.add_benchmark(
        "connectors.jira.webhook",
        webhook_total,
        webhook_started.elapsed(),
    );

    ConnectorMetrics {
        name: "jira",
        initial_events: initial.events.len() as u64,
        incremental_events: incremental.events.len() as u64,
        webhook_events: webhook_total,
        subscriptions: 1,
    }
}

fn ts(secs: i64) -> String {
    format!("{secs}.000000")
}

fn message(channel: &str, ts_str: &str, text: &str) -> SlackMessage {
    SlackMessage {
        ts: ts_str.into(),
        message_type: "message".into(),
        subtype: None,
        channel: channel.into(),
        user: Some("U-DEMO".into()),
        text: text.into(),
    }
}

fn exercise_slack(
    _dataset: &Dataset,
    log: &mut AssertionLog,
    report: &mut DemoReport,
) -> ConnectorMetrics {
    let scope = ScopeId::new_v4();
    let cfg = ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, scope);
    let instance = ConnectorInstanceId::new_v4();

    let now_secs = Utc::now().timestamp();
    let initial_pages = vec![SlackInitialSyncPage {
        channels: vec![SlackChannel {
            id: "C-PRODUCT".into(),
            name: "product".into(),
            is_archived: false,
        }],
        history_by_channel: vec![SlackHistoryResponse {
            ok: true,
            channel: "C-PRODUCT".into(),
            messages: vec![
                message("C-PRODUCT", &ts(now_secs - 7200), "Pinned: roadmap review"),
                message(
                    "C-PRODUCT",
                    &ts(now_secs - 5400),
                    "Decision: ship v2 next quarter",
                ),
                message(
                    "C-PRODUCT",
                    &ts(now_secs - 3600),
                    "Action item: prepare migration guide",
                ),
            ],
            has_more: false,
            response_metadata: SlackResponseMetadata::default(),
        }],
    }];

    let incremental_pages = vec![SlackHistoryResponse {
        ok: true,
        channel: "C-PRODUCT".into(),
        messages: vec![
            SlackMessage {
                ts: ts(now_secs - 1800),
                message_type: "message".into(),
                subtype: Some("message_changed".into()),
                channel: "C-PRODUCT".into(),
                user: Some("U-DEMO".into()),
                text: "Updated decision: pull-in to this quarter".into(),
            },
            SlackMessage {
                ts: ts(now_secs - 900),
                message_type: "message".into(),
                subtype: Some("message_deleted".into()),
                channel: "C-PRODUCT".into(),
                user: Some("U-DEMO".into()),
                text: "Removed: stale pin".into(),
            },
        ],
        has_more: false,
        response_metadata: SlackResponseMetadata::default(),
    }];

    let connector = SlackConnector::new(instance)
        .with_initial_pages(initial_pages)
        .with_incremental_pages(incremental_pages);

    let token = connector.authenticate(&cfg).expect("slack auth");
    log.record(
        PHASE,
        "slack.authenticate returns channels:history scope",
        token.scope.contains("channels:history"),
    );

    let initial_started = Instant::now();
    let initial = connector
        .initial_sync(&cfg, &token)
        .expect("slack initial_sync");
    report.add_benchmark(
        "connectors.slack.initial_sync",
        initial.events.len() as u64,
        initial_started.elapsed(),
    );
    log.record(
        PHASE,
        "slack.initial_sync emits one DocumentCreated per message",
        initial.events.len() == 3
            && initial
                .events
                .iter()
                .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    let mut sync_state = SyncState::new(instance);
    sync_state.cursor.clone_from(&initial.next_cursor);
    sync_state.mode = SyncMode::Incremental;
    sync_state.status = SyncStatus::Succeeded;

    let inc_started = Instant::now();
    let incremental = connector
        .incremental_sync(&cfg, &token, &sync_state)
        .expect("slack incremental_sync");
    report.add_benchmark(
        "connectors.slack.incremental_sync",
        incremental.events.len() as u64,
        inc_started.elapsed(),
    );
    let inc_has_update = incremental
        .events
        .iter()
        .any(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. }));
    let inc_has_delete = incremental
        .events
        .iter()
        .any(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }));
    log.record(
        PHASE,
        "slack.incremental_sync surfaces both update and delete",
        inc_has_update && inc_has_delete,
    );

    let subscription = connector
        .subscribe_webhook(&cfg, &token, "https://demo.example/webhooks/slack")
        .expect("slack webhook subscribe");
    log.record(
        PHASE,
        "slack.subscribe_webhook returns subscription bound to instance",
        subscription.connector == instance,
    );

    let webhook_started = Instant::now();
    let event_payload = serde_json::json!({
        "type": "event_callback",
        "team_id": "T-DEMO",
        "event": {
            "type": "message",
            "channel": "C-PRODUCT",
            "user": "U-DEMO",
            "ts": ts(now_secs - 60),
            "text": "Live event: launch confirmed",
        }
    });
    let webhook_events = connector
        .handle_webhook_event(&serde_json::to_vec(&event_payload).unwrap())
        .expect("slack webhook decode");
    report.add_benchmark(
        "connectors.slack.webhook",
        webhook_events.len() as u64,
        webhook_started.elapsed(),
    );
    log.record(
        PHASE,
        "slack.handle_webhook_event(message) yields DocumentCreated",
        webhook_events
            .iter()
            .any(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    ConnectorMetrics {
        name: "slack",
        initial_events: initial.events.len() as u64,
        incremental_events: incremental.events.len() as u64,
        webhook_events: webhook_events.len() as u64,
        subscriptions: 1,
    }
}

fn exercise_email(
    _dataset: &Dataset,
    log: &mut AssertionLog,
    report: &mut DemoReport,
) -> ConnectorMetrics {
    let scope = ScopeId::new_v4();
    let cfg = ConnectorConfig::new(ConnectorKind::Email, AuthKind::OAuth2, scope);
    let instance = ConnectorInstanceId::new_v4();

    let now = Utc::now();
    let gmail_initial_pages = vec![GmailMessagesListPage {
        messages: vec![
            GmailMessage {
                id: "gmail-msg-1".into(),
                thread_id: "gmail-thread-1".into(),
                internal_date: (now - Duration::hours(48)).timestamp_millis().to_string(),
                history_id: "1001".into(),
            },
            GmailMessage {
                id: "gmail-msg-2".into(),
                thread_id: "gmail-thread-1".into(),
                internal_date: (now - Duration::hours(24)).timestamp_millis().to_string(),
                history_id: "1002".into(),
            },
        ],
        next_page_token: None,
    }];

    let gmail_incremental_pages = vec![GmailMessagesListPage {
        messages: vec![GmailMessage {
            id: "gmail-msg-3".into(),
            thread_id: "gmail-thread-2".into(),
            internal_date: (now - Duration::minutes(30)).timestamp_millis().to_string(),
            history_id: "1101".into(),
        }],
        next_page_token: None,
    }];

    let gmail = EmailConnector::new(instance, EmailProvider::Gmail)
        .with_gmail_initial_pages(gmail_initial_pages)
        .with_gmail_incremental_pages(gmail_incremental_pages);

    let token = gmail.authenticate(&cfg).expect("gmail auth");
    log.record(
        PHASE,
        "email[gmail].authenticate returns gmail.readonly scope",
        token.scope.contains("gmail.readonly"),
    );

    let gmail_initial_started = Instant::now();
    let gmail_initial = gmail
        .initial_sync(&cfg, &token)
        .expect("gmail initial_sync");
    report.add_benchmark(
        "connectors.email.gmail.initial_sync",
        gmail_initial.events.len() as u64,
        gmail_initial_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[gmail].initial_sync emits one DocumentCreated per message",
        gmail_initial.events.len() == 2
            && gmail_initial
                .events
                .iter()
                .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    let mut gmail_state = SyncState::new(instance);
    gmail_state.cursor.clone_from(&gmail_initial.next_cursor);
    gmail_state.mode = SyncMode::Incremental;
    gmail_state.status = SyncStatus::Succeeded;
    let gmail_inc_started = Instant::now();
    let gmail_incremental = gmail
        .incremental_sync(&cfg, &token, &gmail_state)
        .expect("gmail incremental_sync");
    report.add_benchmark(
        "connectors.email.gmail.incremental_sync",
        gmail_incremental.events.len() as u64,
        gmail_inc_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[gmail].incremental_sync emits at least one event",
        !gmail_incremental.events.is_empty(),
    );

    let gmail_subscription = gmail
        .subscribe_webhook(&cfg, &token, "https://demo.example/webhooks/gmail")
        .expect("gmail webhook subscribe");
    log.record(
        PHASE,
        "email[gmail].subscribe_webhook bound to instance",
        gmail_subscription.connector == instance,
    );

    let gmail_webhook_started = Instant::now();
    let gmail_webhook_payload = serde_json::json!({
        "emailAddress": "ops@demo.example",
        "historyId": 1200u64,
        "messageIds": ["gmail-msg-4", "gmail-msg-5"],
    });
    let gmail_webhook_events = gmail
        .handle_webhook_event(&serde_json::to_vec(&gmail_webhook_payload).unwrap())
        .expect("gmail webhook decode");
    report.add_benchmark(
        "connectors.email.gmail.webhook",
        gmail_webhook_events.len() as u64,
        gmail_webhook_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[gmail].handle_webhook_event emits one event per messageId",
        gmail_webhook_events.len() == 2,
    );

    let graph_instance = ConnectorInstanceId::new_v4();
    let graph_initial_pages = vec![GraphMessagesPage {
        value: vec![
            GraphMessage {
                id: "graph-msg-1".into(),
                conversation_id: "conv-1".into(),
                created_date_time: Some(now - Duration::hours(48)),
                last_modified_date_time: Some(now - Duration::hours(48)),
            },
            GraphMessage {
                id: "graph-msg-2".into(),
                conversation_id: "conv-2".into(),
                created_date_time: Some(now - Duration::hours(36)),
                last_modified_date_time: Some(now - Duration::hours(36)),
            },
        ],
        next_link: None,
        delta_link: Some("delta-token-1".into()),
    }];
    let graph_incremental_pages = vec![GraphMessagesPage {
        value: vec![GraphMessage {
            id: "graph-msg-3".into(),
            conversation_id: "conv-2".into(),
            created_date_time: Some(now - Duration::minutes(15)),
            last_modified_date_time: Some(now - Duration::minutes(15)),
        }],
        next_link: None,
        delta_link: Some("delta-token-2".into()),
    }];

    let graph = EmailConnector::new(graph_instance, EmailProvider::MicrosoftGraph)
        .with_graph_initial_pages(graph_initial_pages)
        .with_graph_incremental_pages(graph_incremental_pages);

    let graph_token = graph.authenticate(&cfg).expect("graph auth");
    log.record(
        PHASE,
        "email[graph].authenticate returns Mail.Read scope",
        graph_token.scope.contains("Mail.Read"),
    );

    let graph_initial_started = Instant::now();
    let graph_initial = graph
        .initial_sync(&cfg, &graph_token)
        .expect("graph initial_sync");
    report.add_benchmark(
        "connectors.email.graph.initial_sync",
        graph_initial.events.len() as u64,
        graph_initial_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[graph].initial_sync seeds delta-link cursor",
        graph_initial.next_cursor.as_deref() == Some("delta-token-1"),
    );

    let mut graph_state = SyncState::new(graph_instance);
    graph_state.cursor.clone_from(&graph_initial.next_cursor);
    graph_state.mode = SyncMode::Incremental;
    graph_state.status = SyncStatus::Succeeded;
    let graph_inc_started = Instant::now();
    let graph_incremental = graph
        .incremental_sync(&cfg, &graph_token, &graph_state)
        .expect("graph incremental_sync");
    report.add_benchmark(
        "connectors.email.graph.incremental_sync",
        graph_incremental.events.len() as u64,
        graph_inc_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[graph].incremental_sync emits at least one event",
        !graph_incremental.events.is_empty(),
    );

    let graph_subscription = graph
        .subscribe_webhook(&cfg, &graph_token, "https://demo.example/webhooks/graph")
        .expect("graph webhook subscribe");
    log.record(
        PHASE,
        "email[graph].subscribe_webhook bound to instance",
        graph_subscription.connector == graph_instance,
    );

    let graph_webhook_started = Instant::now();
    let graph_webhook_payload = serde_json::json!({
        "value": [{
            "subscriptionId": graph_subscription.id.0.to_string(),
            "changeType": "created",
            "resource": "/me/messages/graph-msg-9",
            "resourceData": {"id": "graph-msg-9"},
            "subscriptionExpirationDateTime": (Utc::now() + Duration::days(2)).to_rfc3339(),
        }]
    });
    let graph_webhook_events = graph
        .handle_webhook_event(&serde_json::to_vec(&graph_webhook_payload).unwrap())
        .expect("graph webhook decode");
    report.add_benchmark(
        "connectors.email.graph.webhook",
        graph_webhook_events.len() as u64,
        graph_webhook_started.elapsed(),
    );
    log.record(
        PHASE,
        "email[graph].handle_webhook_event emits DocumentCreated for created notifications",
        graph_webhook_events
            .iter()
            .any(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })),
    );

    ConnectorMetrics {
        name: "email",
        initial_events: (gmail_initial.events.len() + graph_initial.events.len()) as u64,
        incremental_events: (gmail_incremental.events.len() + graph_incremental.events.len())
            as u64,
        webhook_events: (gmail_webhook_events.len() + graph_webhook_events.len()) as u64,
        subscriptions: 2,
    }
}
