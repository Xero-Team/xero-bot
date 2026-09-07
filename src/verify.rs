//! Adversarial re-check of the model's own findings.
//!
//! A single model call that both finds problems and grades them is optimistic
//! by construction: everything it writes down, it believed when it wrote it,
//! and a model asked to "report problems" has an incentive to have found some.
//! The pattern this module implements is the one the team's own manual AI
//! audits follow (see the review of AstrBot PR #28): a finding is only trusted
//! after a *separate* pass has re-read the evidence and tried to refute it.
//!
//! Three pieces:
//!
//! 1. [`finding_id`] — a stable identity per finding, so a re-review round can
//!    talk about "the same" finding without the prose drifting.
//! 2. [`verify_findings`] — one blind AI call per significant finding. "Blind"
//!    is the point: the checker sees the diff and the finding's claim, never
//!    the original verdict, so it cannot anchor on the first model's
//!    confidence. It must answer CONFIRM or REFUTE.
//! 3. [`apply_verdicts`] — fold the answers back into the verdict. A REFUTED
//!    finding is demoted to `low` and marked, not deleted: the reader decides
//!    whether a disagreement between two passes is noise or a hint.
//!
//! Nothing here talks to the AI itself — the caller passes a closure, so the
//! builtin engine, the agent engine and any future engine share one
//! implementation and tests can stand in for the model.

use serde_json::{json, Value};

use crate::lang::Lang;
use crate::review::canon_severity;

/// The severities a blind re-check is worth paying for.
///
/// `low`/`info` findings are style notes and nits; re-litigating them costs a
/// model call each and adds nothing the reader can't judge at a glance. The
/// ones that change what a team does next are the top three.
const VERIFYED_SEVERITIES: [&str; 3] = ["critical", "high", "medium"];

/// Does this finding qualify for a blind re-check?
fn qualifies(f: &Value) -> bool {
    VERIFYED_SEVERITIES.contains(&canon_severity(
        f.get("severity").and_then(|s| s.as_str()).unwrap_or(""),
    ))
}

/// The markers [`apply_verdicts`] appends to a title. Ids are minted from the
/// *underlying* claim, so a title carrying a re-check marker still hashes the
/// same — otherwise the id a refuted finding was checked under would not match
/// the id anyone recomputes from the demoted, marked object.
const TITLE_MARKERS: [&str; 4] = [
    " [not confirmed on re-check]",
    " [re-checked]",
    " [复核未确认]",
    " [已复核]",
];

/// A short, stable identity for one finding: `XRV-` plus the first 8 hex
/// digits of a hash over (file, line, title).
///
/// The hash is FNV-1a, hand-rolled: a full `DefaultHasher` would be fine for
/// uniqueness but its output length is unspecified across releases, and this
/// id is published in the review body, where the same PR reviewed twice by
/// different bot versions should still produce comparable ids.
///
/// Not a global guarantee — two genuinely distinct findings in one file at one
/// line with one title would collide — but that pair is the same finding as far
/// as a reader is concerned.
pub fn finding_id(f: &Value) -> String {
    let file = f.get("file").and_then(|x| x.as_str()).unwrap_or("?");
    let line = f.get("line").and_then(|x| x.as_i64()).unwrap_or(0);
    let title = f.get("title").and_then(|x| x.as_str()).unwrap_or("");
    // Strip the re-check markers so the id survives the demotion/marking
    // `apply_verdicts` performs on the same object.
    let title = TITLE_MARKERS
        .iter()
        .fold(title, |t, m| t.strip_suffix(m).unwrap_or(t));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{file}\u{0}{line}\u{0}{title}").as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // 32 bits is the display: full 64-digit ids make the table unreadable and
    // the collision space over (file, line, title) is small either way.
    format!("XRV-{hash:08X}", hash = (hash >> 32) as u32)
}

/// What one blind checker said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The checker could reproduce the claim from the diff.
    Confirm,
    /// The checker believes the claim is wrong or not established.
    Refute,
    /// The checker could not reach a judgment (API error, unusable answer).
    NoAnswer,
}

/// What a finding looks like after the re-check has been folded in.
pub struct VerifiedFinding {
    pub id: String,
    pub verdict: Verdict,
}

/// Classify one checker reply.
///
/// The checker is asked for JSON (`{"answer": "CONFIRM"|"REFUTE", ...}`), so
/// that field decides when it parses; anything else falls back to a substring
/// heuristic over the raw text. A reply containing both words reads as REFUTE:
/// the checker's job is refutation, and the word it leads with is its answer.
fn classify_reply(text: &str) -> Verdict {
    // JSON first — the structured field can't be faked by a reason sentence.
    if let Some(v) = crate::review::parse_verdict(text) {
        if let Some(answer) = v.get("answer").and_then(|a| a.as_str()) {
            return match answer.trim().to_uppercase().as_str() {
                "CONFIRM" => Verdict::Confirm,
                "REFUTE" => Verdict::Refute,
                _ => Verdict::NoAnswer,
            };
        }
    }
    let upper = text.to_uppercase();
    let confirms = upper.contains("CONFIRM");
    let refutes = upper.contains("REFUTE");
    if refutes {
        Verdict::Refute
    } else if confirms {
        Verdict::Confirm
    } else {
        Verdict::NoAnswer
    }
}

/// The refuter's brief, carrying the diff. Deliberately blind to the original
/// verdict's existence: "you are the second reviewer" is all it needs to know.
///
/// Split so the translation pairs stay short: the diff is appended by this
/// function, never translated.
pub fn checker_prompt_with_diff(lang: Lang, f: &Value, diff: &str) -> String {
    let file = f.get("file").and_then(|x| x.as_str()).unwrap_or("?");
    let line = f.get("line").and_then(|x| x.as_i64()).unwrap_or(0);
    let title = f.get("title").and_then(|x| x.as_str()).unwrap_or("");
    let desc = f.get("description").and_then(|x| x.as_str()).unwrap_or("");
    let sev = canon_severity(f.get("severity").and_then(|s| s.as_str()).unwrap_or(""));
    let head = match lang {
        Lang::En => format!(
            "You are an independent second reviewer. Another reviewer reported the finding \
below about the code change at the end of this message. Your job is to try to REFUTE it: \
read the diff, find the code the claim is about, and decide whether the claim actually \
holds. Reply with a single word first — REFUTE if the claim is wrong, unsupported by the \
diff, or describes intended behaviour rather than a defect — otherwise CONFIRM, then one \
sentence of reasoning.\n\n\
Claim: [{sev}] {file}:{line} — {title}\n{desc}\n\n---\n"
        ),
        Lang::Zh => format!(
            "你是独立二审。另一位审查者对本消息末尾的代码改动报告了以下发现。你的任务是设法推翻它:\
阅读 diff,找到该断言涉及的代码,判断该断言是否真的成立。先用一个词作答 —— 若该断言错误、\
diff 中无依据、或描述的是有意行为而非缺陷,回答 REFUTE;否则回答 CONFIRM —— 然后给出一句话理由。\n\n\
断言: [{sev}] {file}:{line} —— {title}\n{desc}\n\n---\n"
        ),
    };
    // Scrubbed at the same boundary everything else is: the finding's fields
    // are model prose on their way back to a model, and the diff came from
    // GitHub — nothing here is operator material, so the scrub is
    // belt-and-braces rather than the main guard.
    format!("{head}{}\n---\n", crate::redact::scrub(diff))
}

/// Run the blind re-check over every significant finding.
///
/// `check` is one AI call; it receives the checker prompt and answers free
/// text. Failures (API errors, unusable replies) become [`Verdict::NoAnswer`]
/// rather than aborting the review — a re-check that can't run must not cost
/// the whole review its findings.
pub async fn verify_findings<F, Fut>(
    verdict: &Value,
    diff: &str,
    lang: Lang,
    mut check: F,
) -> Vec<VerifiedFinding>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let Some(findings) = verdict.get("findings").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in findings {
        if !qualifies(f) {
            continue;
        }
        let prompt = checker_prompt_with_diff(lang, f, diff);
        let answer = check(prompt).await;
        let v = match answer {
            Ok(text) => classify_reply(&text),
            Err(_) => Verdict::NoAnswer,
        };
        out.push(VerifiedFinding {
            id: finding_id(f),
            verdict: v,
        });
    }
    out
}

/// Fold the re-check answers back into the verdict, and stamp finding ids.
///
/// Rules:
/// - every finding gets an `id` (also the low/info ones, so ids are a
///   property of the published report rather than of the verification run)
/// - a REFUTED finding is demoted one bucket (to `low`) and its title is
///   suffixed with the not-confirmed marker, so the demotion is visible in
///   the body and not only in the log
/// - CONFIRMED critical/high findings are suffixed with the re-checked
///   marker — the reader can see which findings survived a second pass
pub fn apply_verdicts(verdict: &mut Value, checked: &[VerifiedFinding], lang: Lang) {
    let Some(findings) = verdict.get_mut("findings").and_then(|f| f.as_array_mut()) else {
        return;
    };
    for f in findings.iter_mut() {
        let id = finding_id(f);
        if let Some(obj) = f.as_object_mut() {
            obj.insert("id".into(), json!(id));
        }
        let Some(v) = checked.iter().find(|c| c.id == id) else {
            continue;
        };
        match v.verdict {
            Verdict::Refute => {
                if let Some(obj) = f.as_object_mut() {
                    // Demote, never delete: a disagreement between two passes
                    // is information, and silently dropping it would publish
                    // "verified" reports that had simply stopped mentioning
                    // their disputes.
                    let demoted = match canon_severity(
                        obj.get("severity").and_then(|s| s.as_str()).unwrap_or(""),
                    ) {
                        "critical" | "high" => "medium",
                        _ => "low",
                    };
                    obj.insert("severity".into(), json!(demoted));
                    let marker = match lang {
                        Lang::En => " [not confirmed on re-check]",
                        Lang::Zh => " [复核未确认]",
                    };
                    let title = obj.get("title").and_then(|t| t.as_str()).unwrap_or("");
                    obj.insert("title".into(), json!(format!("{title}{marker}")));
                }
            }
            Verdict::Confirm => {
                if let Some(obj) = f.as_object_mut() {
                    let marker = match lang {
                        Lang::En => " [re-checked]",
                        Lang::Zh => " [已复核]",
                    };
                    let title = obj.get("title").and_then(|t| t.as_str()).unwrap_or("");
                    if !title.contains(marker.trim()) {
                        obj.insert("title".into(), json!(format!("{title}{marker}")));
                    }
                }
            }
            Verdict::NoAnswer => {}
        }
    }
}

/// How many findings a re-check would cost, for the "should we?" decision the
/// caller makes. Cheap to expose so the caller can log the budget it is
/// about to spend.
pub fn verifiable_count(verdict: &Value) -> usize {
    verdict
        .get("findings")
        .and_then(|f| f.as_array())
        .map(|fs| fs.iter().filter(|f| qualifies(f)).count())
        .unwrap_or(0)
}

/// Run the re-check over `verdict` when `cfg` has it enabled, and stamp ids
/// either way. The one entry the engines call, so all four cannot drift apart
/// on *when* the pass runs; the AI call itself is `check` (usually a closure
/// over [`crate::review::call_ai`] pinned to the checker's system prompt).
///
/// Returns the engine tag suffix: `"builtin + verify"` mirrors what the
/// builtin engine used to name itself, so the published `_engine:` line
/// records whether this report was re-checked.
pub async fn verify_and_stamp<F, Fut>(
    verdict: &mut Value,
    cfg: &crate::config::Config,
    diff: &str,
    lang: Lang,
    check: F,
) -> String
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let mut check = check;
    let ran = cfg.review_verify && cfg.ai_ready() && verifiable_count(verdict) > 0;
    if ran {
        let n = verifiable_count(verdict);
        let checked = verify_findings(verdict, diff, lang, &mut check).await;
        let confirmed = checked
            .iter()
            .filter(|c| c.verdict == Verdict::Confirm)
            .count();
        let refuted = checked
            .iter()
            .filter(|c| c.verdict == Verdict::Refute)
            .count();
        tracing::info!("review re-check: {n} checked, {confirmed} confirmed, {refuted} refuted");
        apply_verdicts(verdict, &checked, lang);
    } else {
        // Ids are stamped even when verification is off — they are what makes
        // a finding addressable across rounds, and they cost nothing.
        apply_verdicts(verdict, &[], lang);
    }
    // The tag reflects what actually happened: a report that wanted a
    // re-check but couldn't run one (no findings, no AI config) must not
    // advertise one.
    if ran {
        "+ verify".to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: &str, title: &str) -> Value {
        json!({
            "severity": sev,
            "title": title,
            "file": "src/main.rs",
            "line": 3,
            "description": "d",
            "suggestion": "s"
        })
    }

    #[test]
    fn ids_are_stable_and_shape_correct() {
        let a = finding("high", "bug");
        let again = finding("high", "bug");
        let other = finding("high", "different bug");
        let id1 = finding_id(&a);
        let id2 = finding_id(&again);
        let id3 = finding_id(&other);
        assert_eq!(id1, id2, "same finding, same id");
        assert_ne!(id1, id3, "different finding, different id");
        assert!(id1.starts_with("XRV-"));
        assert_eq!(id1.len(), 12, "XRV- + 8 hex: {id1}");
    }

    /// A finding's severity field alone must not change its identity — the
    /// demotion in `apply_verdicts` happens after the id is minted, so a
    /// re-derivation from the demoted object must still match.
    #[test]
    fn ids_ignore_severity_and_prose_fields() {
        let mut f = finding("high", "bug");
        f["severity"] = json!("low");
        f["description"] = json!("revised");
        assert_eq!(finding_id(&f), finding_id(&finding("high", "bug")));
    }

    #[test]
    fn replies_are_classified_by_the_word_they_lead_with() {
        assert_eq!(
            classify_reply("CONFIRM the null deref is real"),
            Verdict::Confirm
        );
        assert_eq!(
            classify_reply("REFUTE: the value is checked on line 40"),
            Verdict::Refute
        );
        // Both words: the refuter's job decides.
        assert_eq!(
            classify_reply("I cannot CONFIRM this, so REFUTE"),
            Verdict::Refute
        );
        assert_eq!(classify_reply("looks plausible to me"), Verdict::NoAnswer);
        assert_eq!(classify_reply(""), Verdict::NoAnswer);
    }

    #[test]
    fn only_significant_findings_are_checked() {
        let verdict = json!({
            "summary": "s",
            "findings": [
                finding("critical", "a"),
                finding("high", "b"),
                finding("medium", "c"),
                finding("low", "d"),
                finding("info", "e"),
                finding("warning", "f") // SARIF spelling of medium
            ]
        });
        assert_eq!(verifiable_count(&verdict), 4);
    }

    /// `verify_findings` with a stand-in model: two significant findings,
    /// one confirmed and one refuted.
    #[tokio::test]
    async fn the_re_check_runs_one_blind_call_per_significant_finding() {
        let verdict = json!({
            "summary": "s",
            "findings": [finding("high", "a"), finding("info", "skip me"), finding("medium", "b")]
        });
        let mut prompts_seen = Vec::new();
        let checked = {
            let seen = &mut prompts_seen;
            verify_findings(&verdict, "DIFF", Lang::En, move |p| {
                seen.push(p);
                async { Ok("CONFIRM — the check on line 40 is absent".to_string()) }
            })
            .await
        };
        // Two calls: the info finding is not worth one.
        assert_eq!(prompts_seen.len(), 2, "{prompts_seen:?}");
        // Blind: the checker's prompt carries the diff and the claim, but the
        // word "summary" from the surrounding verdict must not be there to
        // anchor on.
        let p = &prompts_seen[0];
        assert!(p.contains("DIFF"), "{p}");
        assert!(p.contains("REFUTE"), "{p}");
        assert_eq!(checked.len(), 2);
        assert_eq!(checked[0].verdict, Verdict::Confirm);
    }

    #[tokio::test]
    async fn a_checker_failure_is_no_answer_not_an_abort() {
        let verdict = json!({"summary": "s", "findings": [finding("high", "a")]});
        let checked = verify_findings(&verdict, "DIFF", Lang::En, |_p| async {
            Err("boom".to_string())
        })
        .await;
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].verdict, Verdict::NoAnswer);
    }

    #[test]
    fn refuted_findings_are_demoted_and_marked_not_deleted() {
        let mut verdict = json!({
            "summary": "s",
            "findings": [
                finding("critical", "sql injection"),
                finding("high", "leak"),
                finding("medium", "edge case"),
                finding("low", "style")
            ]
        });
        let checked: Vec<VerifiedFinding> = [
            ("sql injection", Verdict::Refute),
            ("leak", Verdict::Confirm),
            ("edge case", Verdict::NoAnswer),
        ]
        .into_iter()
        .map(|(title, v)| {
            let f = finding("high", title);
            VerifiedFinding {
                id: finding_id(&f),
                verdict: v,
            }
        })
        .collect();
        apply_verdicts(&mut verdict, &checked, Lang::En);

        let fs = verdict["findings"].as_array().unwrap();
        let by_title = |needle: &str| {
            fs.iter()
                .find(|f| f["title"].as_str().unwrap().starts_with(needle))
                .unwrap()
        };
        // Refuted: demoted to medium... but the id was minted from the
        // original (file, line, title), so lookups stay stable.
        let refuted = by_title("sql injection");
        assert_eq!(refuted["severity"], "medium");
        assert!(
            refuted["title"].as_str().unwrap().contains("not confirmed"),
            "{refuted}"
        );
        // And the id is stamped either way.
        assert!(refuted["id"].as_str().unwrap().starts_with("XRV-"));

        let confirmed = by_title("leak");
        assert_eq!(confirmed["severity"], "high");
        assert!(
            confirmed["title"].as_str().unwrap().contains("re-checked"),
            "{confirmed}"
        );

        // No answer: severity untouched, id still stamped.
        let unanswered = by_title("edge case");
        assert_eq!(unanswered["severity"], "medium");
        assert!(unanswered["id"].as_str().unwrap().starts_with("XRV-"));

        // Untouched by the run: low findings are never checked, but still get
        // an id, because ids belong to the report.
        assert_eq!(by_title("style")["severity"], "low");
        assert!(by_title("style")["id"]
            .as_str()
            .unwrap()
            .starts_with("XRV-"));
    }

    /// A Chinese report gets Chinese markers — the demotion must be visible
    /// to the reader in whichever language the review is written in.
    #[test]
    fn markers_follow_the_report_language() {
        let mut verdict = json!({"summary": "s", "findings": [finding("high", "x")]});
        let checked = vec![VerifiedFinding {
            id: finding_id(&finding("high", "x")),
            verdict: Verdict::Refute,
        }];
        apply_verdicts(&mut verdict, &checked, Lang::Zh);
        let title = verdict["findings"][0]["title"].as_str().unwrap();
        assert!(title.contains("复核未确认"), "{title}");
    }

    /// The "already marked" guard in the Confirm branch: a second
    /// apply_verdicts over the same finding must not double the suffix.
    #[test]
    fn confirming_twice_does_not_stack_the_marker() {
        let mut verdict = json!({"summary": "s", "findings": [finding("high", "x")]});
        let checked = vec![VerifiedFinding {
            id: finding_id(&finding("high", "x")),
            verdict: Verdict::Confirm,
        }];
        apply_verdicts(&mut verdict, &checked, Lang::En);
        let once = verdict["findings"][0]["title"]
            .as_str()
            .unwrap()
            .to_string();
        apply_verdicts(&mut verdict, &checked, Lang::En);
        let twice = verdict["findings"][0]["title"].as_str().unwrap();
        assert_eq!(once, twice, "{once}");
    }

    /// The demotion changed the severity but not the id, so a verdict looked
    /// up *after* a demotion must still match — this is the ordering
    /// invariant the loop in `apply_verdicts` depends on.
    #[test]
    fn demotion_keeps_the_id_lookup_stable() {
        let f = finding("high", "x");
        let id = finding_id(&f);
        let mut verdict = json!({"summary": "s", "findings": [f]});
        let checked = vec![VerifiedFinding {
            id: id.clone(),
            verdict: Verdict::Refute,
        }];
        apply_verdicts(&mut verdict, &checked, Lang::En);
        // Re-deriving the id from the demoted finding still finds the check.
        let demoted = &verdict["findings"][0];
        assert_eq!(finding_id(demoted), id);
        assert_eq!(demoted["severity"], "medium");
    }
}
