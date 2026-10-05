//! PR monitor fetch probe: compares and measures the per-PR `gh pr view` path
//! against the batched GraphQL path on real pull requests (read-only calls).
//!
//! ```sh
//! # Equivalence: record raw payloads and compare the two parse paths.
//! cargo run --release -p ubra-engine --example prbatch -- capture <out-dir> <pr-url>...
//! # Cost of one sweep over N PRs (wrap in /usr/bin/time -p for CPU).
//! cargo run --release -p ubra-engine --example prbatch -- sweep-old [--threads] <pr-url>...
//! cargo run --release -p ubra-engine --example prbatch -- sweep-new [--threads] <pr-url>...
//! ```
//!
//! `GH` overrides the gh binary (a counting shim, for example).

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use ubra_engine::pr_monitor::{
    BatchRef, VIEW_FIELDS, batch_query, fetch, fetch_batch, parse, parse_batch, parse_threads,
    resolve_gh,
};
use ubra_proto::DateMillis;

const CHUNK: usize = 25;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let gh = std::env::var("GH")
        .ok()
        .or_else(resolve_gh)
        .expect("gh on PATH");
    let mode = args.remove(0);
    let threads = if args.first().is_some_and(|arg| arg == "--threads") {
        args.remove(0);
        true
    } else {
        false
    };
    match mode.as_str() {
        "capture" => capture(&gh, PathBuf::from(args.remove(0)), &args),
        "sweep-old" => {
            let fetched = args
                .iter()
                .filter(|url| fetch(url, &gh, threads).is_some())
                .count();
            println!("sweep-old: {fetched}/{} fetched", args.len());
        }
        "sweep-new" => {
            let refs: Vec<(BatchRef, bool)> = args
                .iter()
                .map(|url| (BatchRef::parse(url).expect("batchable url"), threads))
                .collect();
            let mut fetched = 0;
            let mut fallback = 0;
            for chunk in refs.chunks(CHUNK) {
                for ((target, _), status) in chunk.iter().zip(fetch_batch(chunk, &gh)) {
                    if status.is_some() {
                        fetched += 1;
                    } else {
                        fallback += 1;
                        fetched += fetch(&target.url, &gh, threads).is_some() as usize;
                    }
                }
            }
            println!(
                "sweep-new: {fetched}/{} fetched, {fallback} via per-PR fallback",
                args.len()
            );
        }
        "cost" => cost(&gh, &args, threads),
        other => panic!("unknown mode {other}"),
    }
}

/// GitHub's own rate-limit cost and node count for one batch query over
/// `urls`, and for the per-PR queries it replaces (the batch query with one
/// PR has the same connections as `gh pr view`'s; the thread query is the
/// monitor's own). Read-only.
fn cost(gh: &str, urls: &[String], threads: bool) {
    let run = |requests: &[(BatchRef, bool)]| -> Value {
        let (query, variables) = batch_query(requests);
        let query = query.replacen("){", "){rateLimit{cost nodeCount remaining}", 1);
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ];
        for (flag, variable) in variables {
            args.push(flag.to_owned());
            args.push(variable);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let started = std::time::Instant::now();
        let data = gh_output(gh, &args);
        let response: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        let mut rate = response["data"]["rateLimit"].clone();
        rate["bytes"] = data.len().into();
        rate["ms"] = (started.elapsed().as_millis() as u64).into();
        rate
    };
    let refs: Vec<(BatchRef, bool)> = urls
        .iter()
        .map(|url| (BatchRef::parse(url).expect("batchable url"), threads))
        .collect();
    println!("batch of {}: {}", refs.len(), run(&refs));
    println!(
        "one PR (gh pr view connections): {}",
        run(&[(refs[0].0.clone(), false)])
    );
    let (owner, repo, number) = ubra_engine::pr_monitor::pr_coordinates(&urls[0]).unwrap();
    let data = gh_output(
        gh,
        &[
            "api",
            "graphql",
            "-f",
            "query=query($owner:String!,$name:String!,$number:Int!){rateLimit{cost nodeCount remaining} repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100){totalCount nodes{isResolved}}}}}",
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={repo}"),
            "-F",
            &format!("number={number}"),
        ],
    );
    let response: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
    println!("thread query: {}", response["data"]["rateLimit"]);
}

fn gh_output(gh: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(gh)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .expect("run gh");
    output.stdout
}

/// Records `gh pr view` + the per-PR thread query, then the batch, then the
/// per-PR pair again. Live PRs move (CI runs finish, GitHub computes
/// mergeability lazily on first ask), so a PR counts as proven only when
/// both per-PR snapshots agree and the batch matches them.
fn capture(gh: &str, out: PathBuf, urls: &[String]) {
    std::fs::create_dir_all(&out).unwrap();
    let at = DateMillis(0.0);
    let single = |pass: usize| -> Vec<Option<ubra_proto::PullRequestStatus>> {
        urls.iter()
            .map(|url| {
                let (owner, repo, number) = ubra_engine::pr_monitor::pr_coordinates(url).unwrap();
                let view = gh_output(gh, &["pr", "view", url, "--json", VIEW_FIELDS]);
                std::fs::write(out.join(format!("view-{number}-{pass}.json")), &view).unwrap();
                let threads = gh_output(
                    gh,
                    &[
                        "api",
                        "graphql",
                        "-f",
                        "query=query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100){totalCount nodes{isResolved}}}}}",
                        "-f",
                        &format!("owner={owner}"),
                        "-f",
                        &format!("name={repo}"),
                        "-F",
                        &format!("number={number}"),
                    ],
                );
                std::fs::write(out.join(format!("threads-{number}-{pass}.json")), &threads)
                    .unwrap();
                let mut status = parse(&view, url, at);
                if let (Some(status), Some((resolved, total))) =
                    (status.as_mut(), parse_threads(&threads))
                {
                    status.resolved_threads = Some(resolved);
                    status.total_threads = Some(total);
                }
                status
            })
            .collect()
    };
    let before = single(0);
    let refs: Vec<(BatchRef, bool)> = urls
        .iter()
        .map(|url| (BatchRef::parse(url).expect("batchable url"), true))
        .collect();
    let mut batched = Vec::new();
    for (index, chunk) in refs.chunks(CHUNK).enumerate() {
        let (query, variables) = batch_query(chunk);
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ];
        for (flag, variable) in variables {
            args.push(flag.to_owned());
            args.push(variable);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let data = gh_output(gh, &args);
        std::fs::write(out.join(format!("batch-{index}.json")), &data).unwrap();
        batched.extend(parse_batch(&data, chunk, at));
    }
    let after = single(1);
    let (mut same, mut moved) = (0, 0);
    for (((url, a), a2), b) in urls.iter().zip(before).zip(after).zip(batched) {
        let number = BatchRef::parse(url).unwrap().number;
        if b.is_none() {
            println!("#{number}: batch unresolved (per-PR fallback)");
            continue;
        }
        if a != a2 {
            moved += 1;
            println!(
                "#{number}: moved between per-PR snapshots; batch matches one: {}",
                b == a || b == a2
            );
            continue;
        }
        if a == b {
            same += 1;
            let a = a.unwrap();
            println!(
                "#{number}: identical ({} {} {:?}/{:?} checks {}/{}/{} comments {} reviews {} threads {:?}/{:?})",
                a.state,
                if a.is_draft { "draft" } else { "ready" },
                a.mergeable,
                a.review_decision,
                a.checks_passed,
                a.checks_failed,
                a.checks_pending,
                a.comment_count,
                a.review_count,
                a.resolved_threads,
                a.total_threads
            );
            continue;
        }
        println!("#{number}: DIFFERENT");
        let a = serde_json::to_value(&a).unwrap();
        let b = serde_json::to_value(&b).unwrap();
        if let (Value::Object(a), Value::Object(b)) = (&a, &b) {
            for (key, value) in a {
                if b.get(key) != Some(value) {
                    println!("  {key}: view={value} batch={:?}", b.get(key));
                }
            }
        }
    }
    println!(
        "{same}/{} identical, {moved} moved while capturing",
        urls.len()
    );
}
