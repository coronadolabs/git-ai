use crate::ci::ci_context::{CiContext, CiEvent};
use crate::error::GitAiError;
use crate::git::repository::exec_git;
use crate::git::repository::find_repository_in_path;
use crate::metrics::pos_encoded::PosEncoded;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const GITHUB_CI_TEMPLATE_YAML: &str = include_str!("workflow_templates/github.yaml");
const GITHUB_PUSH_METRICS_TEMPLATE_YAML: &str =
    include_str!("workflow_templates/github_push_metrics.yaml");

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct GithubCiEventPayload {
    #[serde(default)]
    pull_request: Option<GithubCiPullRequest>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct GithubCiPullRequest {
    number: u32,
    base: GithubCiPullRequestReference,
    head: GithubCiPullRequestReference,
    merged: bool,
    merge_commit_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct GithubCiPullRequestReference {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    repo: GithubCiRepository,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct GithubCiRepository {
    clone_url: String,
}

pub fn get_github_ci_context() -> Result<Option<CiContext>, GitAiError> {
    let env_event_name = std::env::var("GITHUB_EVENT_NAME").unwrap_or_default();
    let env_event_path = std::env::var("GITHUB_EVENT_PATH").unwrap_or_default();

    if env_event_name != "pull_request" {
        return Ok(None);
    }

    let event_payload =
        serde_json::from_str::<GithubCiEventPayload>(&std::fs::read_to_string(env_event_path)?)
            .unwrap_or_default();
    if event_payload.pull_request.is_none() {
        return Ok(None);
    }

    let pull_request = event_payload.pull_request.unwrap();

    if !pull_request.merged || pull_request.merge_commit_sha.is_none() {
        return Ok(None);
    }

    let pr_number = pull_request.number;
    let head_ref = pull_request.head.ref_name;
    let head_sha = pull_request.head.sha;
    let base_ref = pull_request.base.ref_name;
    let clone_url = pull_request.base.repo.clone_url.clone();

    let clone_dir = "git-ai-ci-clone".to_string();

    // Authenticate the clone URL with GITHUB_TOKEN if available
    let authenticated_url = if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        // Replace https://github.com/ with https://x-access-token:TOKEN@github.com/
        // Supports both public and enterprise github instances.
        format!(
            "https://x-access-token:{}@{}",
            token,
            clone_url.strip_prefix("https://").unwrap_or(&clone_url)
        )
    } else {
        clone_url
    };

    // Clone the repo
    exec_git(&[
        "clone".to_string(),
        "--branch".to_string(),
        base_ref.clone(),
        authenticated_url.clone(),
        clone_dir.clone(),
    ])?;

    // Fetch PR commits using GitHub's special PR refs
    // This is necessary because the PR branch may be deleted after merge
    // but GitHub keeps the commits accessible via pull/{number}/head
    // We store the fetched commits in a local ref to ensure they're kept
    exec_git(&[
        "-C".to_string(),
        clone_dir.clone(),
        "fetch".to_string(),
        authenticated_url.clone(),
        format!("pull/{}/head:refs/github/pr/{}", pr_number, pr_number),
    ])?;

    let repo = find_repository_in_path(&clone_dir.clone())?;

    Ok(Some(CiContext {
        repo,
        event: CiEvent::Merge {
            merge_commit_sha: pull_request.merge_commit_sha.unwrap(),
            head_ref: head_ref.clone(),
            head_sha: head_sha.clone(),
            base_ref: base_ref.clone(),
            base_sha: pull_request.base.sha.clone(),
        },
        temp_dir: PathBuf::from(clone_dir),
    }))
}

/// Install or update the GitHub Actions workflow in the current repository
/// Writes the embedded template to .github/workflows/git-ai.yaml at the repo root
pub fn install_github_ci_workflow() -> Result<PathBuf, GitAiError> {
    // Discover repository at current working directory
    let repo = find_repository_in_path(".")?;
    let workdir = repo.workdir()?;

    // Ensure destination directory exists
    let workflows_dir = workdir.join(".github").join("workflows");
    fs::create_dir_all(&workflows_dir)
        .map_err(|e| GitAiError::Generic(format!("Failed to create workflows dir: {}", e)))?;

    // Write template
    let dest_path = workflows_dir.join("git-ai.yaml");
    fs::write(&dest_path, GITHUB_CI_TEMPLATE_YAML)
        .map_err(|e| GitAiError::Generic(format!("Failed to write workflow file: {}", e)))?;

    Ok(dest_path)
}

/// Install the push-metrics workflow to .github/workflows/git-ai-metrics.yaml
pub fn install_github_push_metrics_workflow() -> Result<PathBuf, GitAiError> {
    let repo = find_repository_in_path(".")?;
    let workdir = repo.workdir()?;

    let workflows_dir = workdir.join(".github").join("workflows");
    fs::create_dir_all(&workflows_dir)
        .map_err(|e| GitAiError::Generic(format!("Failed to create workflows dir: {}", e)))?;

    let dest_path = workflows_dir.join("git-ai-metrics.yaml");
    fs::write(&dest_path, GITHUB_PUSH_METRICS_TEMPLATE_YAML)
        .map_err(|e| GitAiError::Generic(format!("Failed to write workflow file: {}", e)))?;

    Ok(dest_path)
}

/// Run the push-metrics job: walk commits in the push range, compute stats from git notes,
/// and forward them to the configured OTel endpoint.
///
/// Reads from environment by default:
///   GITHUB_BEFORE      – SHA before the push (all-zeros for new branches)
///   GITHUB_SHA         – SHA after the push
///   GITHUB_REF_NAME    – branch name
///   GITHUB_SERVER_URL  – e.g. https://github.com
///   GITHUB_REPOSITORY  – e.g. org/repo
///   GIT_AI_OTEL_ENDPOINT – OTel collector base URL
///
/// All values can be overridden with --before, --after, --branch, --repo-url,
/// --otel-endpoint flags.
pub fn run_github_push_metrics(args: &[String]) -> Result<usize, GitAiError> {
    // --- Flag helpers ---
    let flag = |name: &str| -> Option<String> {
        let mut i = 0;
        while i + 1 < args.len() {
            if args[i] == name {
                return Some(args[i + 1].clone());
            }
            i += 1;
        }
        None
    };

    // --- Resolve inputs (flags first, then env vars) ---
    let before_sha = flag("--before")
        .or_else(|| std::env::var("GITHUB_BEFORE").ok())
        .unwrap_or_default();

    let after_sha = flag("--after")
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .unwrap_or_default();

    let branch = flag("--branch")
        .or_else(|| std::env::var("GITHUB_REF_NAME").ok())
        .unwrap_or_default();

    let repo_url = flag("--repo-url").or_else(|| {
        let server = std::env::var("GITHUB_SERVER_URL").ok()?;
        let repository = std::env::var("GITHUB_REPOSITORY").ok()?;
        Some(format!("{}/{}", server, repository))
    });

    let otel_endpoint = flag("--otel-endpoint")
        .or_else(|| crate::config::Config::get().otel_endpoint().map(str::to_string));

    // --- Validate required inputs ---
    if after_sha.is_empty() {
        return Err(GitAiError::Generic(
            "No after SHA. Set GITHUB_SHA or pass --after <sha>".to_string(),
        ));
    }
    let Some(otel_endpoint) = otel_endpoint else {
        return Err(GitAiError::Generic(
            "No OTel endpoint configured. Set GIT_AI_OTEL_ENDPOINT or pass --otel-endpoint <url>"
                .to_string(),
        ));
    };

    let repo_url = repo_url.unwrap_or_default();

    println!(
        "git-ai metrics: {}..{} branch={} repo={}",
        if before_sha.is_empty() { "(none)" } else { &before_sha },
        &after_sha,
        branch,
        repo_url,
    );
    println!("OTel endpoint: {}", otel_endpoint);

    // --- Open repo ---
    let repo = find_repository_in_path(".")?;

    // --- Fetch notes (non-fatal) ---
    print!("Fetching authorship notes... ");
    match crate::git::sync_authorship::fetch_authorship_notes(&repo, "origin") {
        Ok(_) => println!("ok"),
        Err(e) => println!("skipped ({})", e),
    }

    // --- Collect commit SHAs in push range ---
    const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
    let commit_shas: Vec<String> = if before_sha.is_empty() || before_sha == ZERO_SHA {
        // New branch or force-push without history: process only the tip commit
        vec![after_sha.clone()]
    } else {
        let mut rev_args = repo.global_args_for_exec();
        rev_args.extend([
            "rev-list".to_string(),
            "--reverse".to_string(),
            format!("{}..{}", before_sha, after_sha),
        ]);
        match exec_git(&rev_args) {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            Err(_) => vec![after_sha.clone()],
        }
    };

    println!("Processing {} commit(s)...", commit_shas.len());

    // --- Build MetricEvents for commits that have AI authorship notes ---
    let mut events: Vec<crate::metrics::MetricEvent> = Vec::new();

    for sha in &commit_shas {
        // Skip if no authorship note
        if crate::git::refs::get_authorship(&repo, sha).is_none() {
            continue;
        }

        let stats = match crate::authorship::stats::stats_for_commit_stats(&repo, sha, &[]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  skip {} – stats error: {}", &sha[..8.min(sha.len())], e);
                continue;
            }
        };

        // Only emit if there is AI activity
        if stats.ai_additions == 0 && stats.ai_accepted == 0 {
            continue;
        }

        // Get author email via git show
        let author_email = get_commit_author_email(&repo, sha);

        // Build parallel arrays (index 0 = "all" aggregate, 1+ = per tool/model)
        let mut tool_model_pairs: Vec<String> = vec!["all".to_string()];
        let mut mixed_vec: Vec<u32> = vec![stats.mixed_additions];
        let mut ai_add_vec: Vec<u32> = vec![stats.ai_additions];
        let mut ai_acc_vec: Vec<u32> = vec![stats.ai_accepted];
        let mut total_add_vec: Vec<u32> = vec![stats.total_ai_additions];
        let mut total_del_vec: Vec<u32> = vec![stats.total_ai_deletions];
        let mut wait_vec: Vec<u64> = vec![stats.time_waiting_for_ai];

        for (tool_model, ts) in &stats.tool_model_breakdown {
            tool_model_pairs.push(tool_model.clone());
            mixed_vec.push(ts.mixed_additions);
            ai_add_vec.push(ts.ai_additions);
            ai_acc_vec.push(ts.ai_accepted);
            total_add_vec.push(ts.total_ai_additions);
            total_del_vec.push(ts.total_ai_deletions);
            wait_vec.push(ts.time_waiting_for_ai);
        }

        let values = crate::metrics::CommittedValues::new()
            .human_additions(stats.human_additions)
            .git_diff_deleted_lines(stats.git_diff_deleted_lines)
            .git_diff_added_lines(stats.git_diff_added_lines)
            .tool_model_pairs(tool_model_pairs)
            .mixed_additions(mixed_vec)
            .ai_additions(ai_add_vec)
            .ai_accepted(ai_acc_vec)
            .total_ai_additions(total_add_vec)
            .total_ai_deletions(total_del_vec)
            .time_waiting_for_ai(wait_vec)
            .first_checkpoint_ts_null()
            .commit_subject_null()
            .commit_body_null();

        let attrs = crate::metrics::EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
            .repo_url(&repo_url)
            .author(&author_email)
            .commit_sha(sha)
            .branch(&branch);

        let event = crate::metrics::MetricEvent::new(&values, attrs.to_sparse());
        events.push(event);

        println!(
            "  {} ai_additions={} ai_accepted={}",
            &sha[..8.min(sha.len())],
            stats.ai_additions,
            stats.ai_accepted,
        );
    }

    if events.is_empty() {
        println!("No AI-attributed commits found in this push.");
        return Ok(0);
    }

    // --- Send to OTel ---
    println!("Sending {} event(s) to {}...", events.len(), otel_endpoint);
    crate::otel::send_to_otel(&otel_endpoint, &events).map_err(GitAiError::Generic)?;
    println!("Done.");

    Ok(events.len())
}

/// Returns the author email for the given commit SHA using `git show`.
fn get_commit_author_email(repo: &crate::git::repository::Repository, sha: &str) -> String {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%ae".to_string(),
        sha.to_string(),
    ]);
    exec_git(&args)
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
