use super::{TranscriptUsageStore, UsageProvider};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

fn append(path: &Path, records: &[Value]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn codex_event(total: i64, window: Option<i64>) -> Value {
    json!({"timestamp":"2026-07-22T11:10:00Z","type":"event_msg","payload":{
        "type":"token_count","info":{"total_token_usage":{"input_tokens":total,"cached_input_tokens":20,"output_tokens":10},
        "last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"total_tokens":110},
        "model_context_window":window}}})
}

fn claude_event(id: &str, model: &str) -> Value {
    json!({"type":"assistant","timestamp":"2026-07-22T11:10:00Z","requestId":id,
        "message":{"id":id,"model":model,"usage":{"input_tokens":100,"output_tokens":10,
        "cache_read_input_tokens":20,"cache_creation_input_tokens":5}}})
}

#[test]
fn session_projection_excludes_unrelated_files_and_deduplicates_provider_records() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.jsonl");
    let unrelated = dir.path().join("unrelated.jsonl");
    let event = claude_event("m1", "claude-sonnet-4-20250514");
    append(&target, &[event.clone(), event]);
    append(
        &unrelated,
        &[claude_event("other", "claude-sonnet-4-20250514")],
    );
    let mut ledger = TranscriptUsageStore::default();
    let first = ledger
        .refresh(
            "claude:target",
            UsageProvider::Claude,
            &File::open(&target).unwrap(),
        )
        .unwrap();
    assert_eq!(first.tokens.input, 100);
    assert_eq!(first.tokens.output, 10);
    assert_eq!(first.tokens.cache_read, 20);
    assert_eq!(first.tokens.cache_write, 5);
    assert_eq!(
        ledger
            .refresh(
                "claude:target",
                UsageProvider::Claude,
                &File::open(&target).unwrap()
            )
            .unwrap(),
        first
    );
    assert_eq!(first.pricing.priced_tokens, 135);
    assert!(first.pricing.estimated_cost_usd.unwrap() > 0.0);
}

#[test]
fn codex_incremental_duplicates_compaction_and_missing_window_preserve_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    let event = codex_event(100, Some(200_000));
    append(
        &path,
        &[
            json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            event.clone(),
            event.clone(),
        ],
    );
    let mut ledger = TranscriptUsageStore::default();
    let first = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(
        (
            first.tokens.input,
            first.tokens.cache_read,
            first.tokens.output
        ),
        (80, 20, 10)
    );
    assert_eq!(first.context.unwrap().tokens, 110);
    append(
        &path,
        &[
            event,
            json!({"type":"event_msg","payload":{"type":"context_compacted"}}),
        ],
    );
    let compacted = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(compacted.tokens, first.tokens);
    assert!(compacted.context.is_none());
    append(&path, &[codex_event(100, Some(200_000))]);
    let replay = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert!(replay.context.is_none());
    assert_eq!(replay.tokens, first.tokens);
    append(&path, &[codex_event(200, None)]);
    let next = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(next.tokens.input, 160);
    assert_eq!(next.context.unwrap().tokens, 110);
    assert_eq!(next.context.unwrap().window, None);
}

#[test]
fn older_cumulative_replay_after_compaction_does_not_restore_context_or_spend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    append(
        &path,
        &[
            json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            codex_event(100, Some(200_000)),
            codex_event(200, Some(200_000)),
            json!({"type":"event_msg","payload":{"type":"context_compacted"}}),
        ],
    );
    let mut ledger = TranscriptUsageStore::default();
    let compacted = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert!(compacted.context.is_none());
    append(&path, &[codex_event(100, Some(200_000))]);
    let replay = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert!(replay.context.is_none());
    assert_eq!(replay.tokens, compacted.tokens);
    assert_eq!(replay.pricing, compacted.pricing);
    append(&path, &[codex_event(300, Some(200_000))]);
    let fresh = ledger
        .refresh(
            "codex:one",
            UsageProvider::Codex,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(fresh.context.unwrap().tokens, 110);
    assert_eq!(fresh.tokens.input, 240);
}

#[test]
fn pricing_unknown_is_not_zero_and_mixed_models_preserve_partial_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    append(&path, &[claude_event("m1", "unpriced-future-model")]);
    let mut ledger = TranscriptUsageStore::default();
    let unknown = ledger
        .refresh(
            "claude:one",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(unknown.pricing.estimated_cost_usd, None);
    assert_eq!(unknown.pricing.priced_tokens, 0);
    assert_eq!(unknown.pricing.total_tokens, 135);
    append(&path, &[claude_event("m2", "claude-sonnet-4-20250514")]);
    let mixed = ledger
        .refresh(
            "claude:one",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(mixed.pricing.priced_tokens, 135);
    assert_eq!(mixed.pricing.total_tokens, 270);
    assert!(mixed.pricing.estimated_cost_usd.unwrap() > 0.0);
}

#[test]
fn same_conversation_resume_includes_old_history_without_double_charge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    append(&path, &[claude_event("m1", "claude-sonnet-4-20250514")]);
    let mut ledger = TranscriptUsageStore::default();
    let incarnation_a = ledger
        .refresh(
            "claude:resumed",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    let incarnation_b = ledger
        .refresh(
            "claude:resumed",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(incarnation_b, incarnation_a);
    append(&path, &[claude_event("m2", "claude-sonnet-4-20250514")]);
    let resumed = ledger
        .refresh(
            "claude:resumed",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(resumed.tokens.input, 200);
}

#[test]
fn rewritten_transcript_resets_ledger_and_incomplete_tail_is_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation.jsonl");
    append(&path, &[claude_event("first", "unpriced-future-model")]);
    let mut ledger = TranscriptUsageStore::default();
    assert_eq!(
        ledger
            .refresh(
                "claude:one",
                UsageProvider::Claude,
                &File::open(&path).unwrap()
            )
            .unwrap()
            .tokens
            .input,
        100
    );
    std::fs::write(&path, b"{\"type\":\"assistant\"").unwrap();
    let reset = ledger
        .refresh(
            "claude:one",
            UsageProvider::Claude,
            &File::open(&path).unwrap(),
        )
        .unwrap();
    assert_eq!(reset.tokens.input, 0);
    assert_eq!(reset.pricing.estimated_cost_usd, None);
}
