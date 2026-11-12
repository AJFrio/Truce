//! Per-hunk resolution: System One first, then the code model.
//!
//! A confident `ours`, `theirs`, or `both` answer is turned into text in
//! Rust and checked with [`validate_resolution`]. The hunk escalates to
//! chat completions on the code model when:
//!
//! * the choice is `needs_code` (`needs_code`)
//! * a side choice is below `--min-confidence` (`low_confidence`)
//! * the choice is not one of the four options (`unknown_choice`)
//! * the decision API returns a non-auth error or a malformed body
//!   (`decision_error`)
//! * the hunk does not fit the decision context window (`too_large`)
//!
//! 401/403 abort the run. The code model uses the same API key, so those
//! statuses are not escalated. Other decision failures are per hunk.
//! `--concurrency` limits in-flight hunks; a hunk may perform a decision
//! call and then a code call while it holds one permit.

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::config::Settings;
use crate::error::{Result, TruceError};
use crate::openrouter::OpenRouterClient;
use crate::output::{FileScan, FileStatus};
use crate::parse::Hunk;
use crate::resolve::{
    materialize_choice, validate_resolution, AttemptInfo, DecisionInfo, EscalationReason,
    HunkOutcome, ResolverPath,
};
use crate::systemone::{
    prepare_request, DecisionAnswer, DecisionClass, PreparedRequest, SystemOneClient,
};
use crate::transport::Transport;

pub(crate) async fn resolve_scans(scans: &mut [FileScan], settings: &Settings) -> Result<()> {
    let transport = Transport::new(settings)?;
    let decision = SystemOneClient::from_parts(transport.clone(), settings.model.clone());
    let code = OpenRouterClient::from_parts(transport, settings.code_model.clone());

    struct Job {
        file: usize,
        hunk: usize,
        path: String,
        hunk_data: Hunk,
    }

    let mut jobs = Vec::new();
    for (file, scan) in scans.iter().enumerate() {
        if scan.status != FileStatus::Conflicted {
            continue;
        }
        let path = scan.rel_path.to_string_lossy().replace('\\', "/");
        for (hunk_idx, hunk) in scan.hunks.iter().enumerate() {
            jobs.push(Job {
                file,
                hunk: hunk_idx,
                path: path.clone(),
                hunk_data: hunk.clone(),
            });
        }
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let sem = Arc::new(Semaphore::new(settings.concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for job in jobs {
        let decision = decision.clone();
        let code = code.clone();
        let sem = sem.clone();
        let min_confidence = settings.min_confidence;
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore");
            let outcome =
                resolve_hunk(&decision, &code, &job.path, &job.hunk_data, min_confidence).await;
            (job.file, job.hunk, outcome)
        });
    }

    while let Some(joined) = set.join_next().await {
        let (file, hunk, outcome) =
            joined.map_err(|err| TruceError::OpenRouter(format!("worker task failed: {err}")))?;
        let (result, attempt) = outcome?;
        if let Some(scan) = scans.get_mut(file) {
            if let Some(slot) = scan.outcomes.get_mut(hunk) {
                *slot = result;
            }
            if let Some(slot) = scan.attempts.get_mut(hunk) {
                *slot = attempt;
            }
        }
    }
    Ok(())
}

async fn resolve_hunk(
    decision: &SystemOneClient,
    code: &OpenRouterClient,
    path: &str,
    hunk: &Hunk,
    min_confidence: f64,
) -> Result<(HunkOutcome, AttemptInfo)> {
    match prepare_request(decision.model(), path, hunk) {
        PreparedRequest::TooLarge { .. } => {
            escalate(
                code,
                path,
                hunk,
                min_confidence,
                EscalationReason::TooLarge,
                None,
            )
            .await
        }
        PreparedRequest::Ready(body) => match decision.decide(&body).await {
            Err(TruceError::OpenRouterAuth) => Err(TruceError::OpenRouterAuth),
            Err(TruceError::MissingApiKey) => Err(TruceError::MissingApiKey),
            Err(TruceError::Decision(_)) => {
                escalate(
                    code,
                    path,
                    hunk,
                    min_confidence,
                    EscalationReason::DecisionError,
                    None,
                )
                .await
            }
            Err(err) => Err(err),
            Ok(answer) => apply_decision(code, path, hunk, min_confidence, answer).await,
        },
    }
}

async fn apply_decision(
    code: &OpenRouterClient,
    path: &str,
    hunk: &Hunk,
    min_confidence: f64,
    answer: DecisionAnswer,
) -> Result<(HunkOutcome, AttemptInfo)> {
    let info = answer.info();
    match answer.classify() {
        DecisionClass::NeedsCode => {
            escalate(
                code,
                path,
                hunk,
                min_confidence,
                EscalationReason::NeedsCode,
                Some(info),
            )
            .await
        }
        DecisionClass::Unknown => {
            escalate(
                code,
                path,
                hunk,
                min_confidence,
                EscalationReason::UnknownChoice,
                Some(info),
            )
            .await
        }
        DecisionClass::Side(_) if answer.confidence < min_confidence => {
            escalate(
                code,
                path,
                hunk,
                min_confidence,
                EscalationReason::LowConfidence,
                Some(info),
            )
            .await
        }
        DecisionClass::Side(choice) => {
            let (text, rationale) = materialize_choice(choice, &hunk.ours, &hunk.theirs);
            let attempt = AttemptInfo {
                decision: Some(info),
                resolver: Some(ResolverPath::Decision),
                escalation_reason: None,
            };
            match validate_resolution(
                text,
                choice,
                rationale,
                answer.confidence,
                min_confidence,
                &hunk.ours,
                &hunk.theirs,
            ) {
                Ok(resolution) => Ok((HunkOutcome::Resolved(resolution), attempt)),
                Err(reason) => Ok((HunkOutcome::Rejected(reason), attempt)),
            }
        }
    }
}

async fn escalate(
    code: &OpenRouterClient,
    path: &str,
    hunk: &Hunk,
    min_confidence: f64,
    reason: EscalationReason,
    decision: Option<DecisionInfo>,
) -> Result<(HunkOutcome, AttemptInfo)> {
    let attempt = AttemptInfo {
        decision,
        resolver: Some(ResolverPath::Code),
        escalation_reason: Some(reason),
    };
    match code.resolve_hunk(path, hunk, min_confidence).await? {
        HunkOutcome::Resolved(resolution) => Ok((HunkOutcome::Resolved(resolution), attempt)),
        HunkOutcome::Rejected(reason) => Ok((HunkOutcome::Rejected(reason), attempt)),
        HunkOutcome::Unresolved => Ok((
            HunkOutcome::Rejected("code model did not return a resolution".into()),
            attempt,
        )),
    }
}
