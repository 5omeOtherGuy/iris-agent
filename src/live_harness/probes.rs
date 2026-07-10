//! Recall-probe bank and mechanical scoring (goal 7). A probe asks a fixed
//! question about material the agent saw BEFORE compaction landed, with an
//! answer key matched mechanically -- no subjective grading. A second signal,
//! the count of re-fetches of already-seen sources after the probe phase,
//! measures "lostness": if the agent has to re-read what it was told, the
//! compaction lost it.
//!
//! The bank ships small/stub here; the SCORING mechanics are the real,
//! unit-tested contract so R2 (repo Q&A with recall probes) can plug a larger
//! bank in without reworking measurement.

/// One recall probe: a question, the accepted answer fragments (case-insensitive
/// substring match, any one suffices), and the source whose later re-read counts
/// as a re-fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    pub(crate) id: &'static str,
    pub(crate) question: &'static str,
    pub(crate) answer_key: &'static [&'static str],
    /// The workspace-relative source that carried the answer. A read of this
    /// path after the probe phase is a re-fetch (lostness signal).
    pub(crate) source_ref: &'static str,
}

impl Probe {
    /// True when `answer` contains any accepted key fragment, case-insensitive.
    pub(crate) fn is_answered_by(&self, answer: &str) -> bool {
        let hay = answer.to_ascii_lowercase();
        self.answer_key
            .iter()
            .any(|key| hay.contains(&key.to_ascii_lowercase()))
    }
}

/// A fixed set of probes scored as a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeBank {
    pub(crate) probes: Vec<Probe>,
}

/// The mechanical score for one run's probe answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeScore {
    pub(crate) matched: usize,
    pub(crate) total: usize,
    /// Re-reads of probe sources after the probe phase -- the lostness signal.
    pub(crate) refetches: usize,
}

impl ProbeScore {
    /// Fraction answered, or `None` for an empty bank.
    pub(crate) fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.matched as f64 / self.total as f64)
    }
}

impl ProbeBank {
    /// Score `answers` (probe id -> the agent's answer text) against the key and
    /// count re-fetches among `post_reads` (workspace-relative read targets
    /// issued after the probe phase). A missing answer scores as unmatched; an
    /// answer for an unknown id is ignored.
    pub(crate) fn score(&self, answers: &[(String, String)], post_reads: &[String]) -> ProbeScore {
        let matched = self
            .probes
            .iter()
            .filter(|probe| {
                answers
                    .iter()
                    .find(|(id, _)| id == probe.id)
                    .is_some_and(|(_, answer)| probe.is_answered_by(answer))
            })
            .count();
        let sources: std::collections::HashSet<&str> =
            self.probes.iter().map(|p| p.source_ref).collect();
        let refetches = post_reads
            .iter()
            .filter(|target| sources.contains(target.as_str()))
            .count();
        ProbeScore {
            matched,
            total: self.probes.len(),
            refetches,
        }
    }
}

/// A small stub bank for the pilot: two probes over synthetic planted facts.
/// Real R2 banks replace this without touching [`ProbeBank::score`].
pub(crate) fn pilot_probe_bank() -> ProbeBank {
    ProbeBank {
        probes: vec![
            Probe {
                id: "flag",
                question: "What flag did the earlier NEEDLE fact record?",
                answer_key: &["--enable-zeta"],
                source_ref: "NEEDLE.txt",
            },
            Probe {
                id: "target",
                question: "Which function was the reconciliation target?",
                answer_key: &["reconcile_ledger"],
                source_ref: "telemetry/sink.rs",
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_key_matches_case_insensitively_on_any_fragment() {
        let probe = Probe {
            id: "flag",
            question: "?",
            answer_key: &["--enable-zeta", "enable zeta"],
            source_ref: "NEEDLE.txt",
        };
        assert!(probe.is_answered_by("The flag is --ENABLE-ZETA, exactly."));
        assert!(probe.is_answered_by("you enable zeta here"));
        assert!(!probe.is_answered_by("I do not remember the flag."));
    }

    #[test]
    fn score_counts_matches_and_refetches() {
        let bank = pilot_probe_bank();
        let answers = vec![
            ("flag".to_string(), "it was --enable-zeta".to_string()),
            ("target".to_string(), "no idea".to_string()),
        ];
        // One re-fetch of a probe source, one unrelated read.
        let post_reads = vec!["NEEDLE.txt".to_string(), "Cargo.toml".to_string()];
        let score = bank.score(&answers, &post_reads);
        assert_eq!(score.matched, 1);
        assert_eq!(score.total, 2);
        assert_eq!(score.refetches, 1);
        assert_eq!(score.fraction(), Some(0.5));
    }

    #[test]
    fn missing_and_unknown_answers_are_handled() {
        let bank = pilot_probe_bank();
        // Only an unknown id is supplied; nothing matches, no re-fetches.
        let answers = vec![("mystery".to_string(), "--enable-zeta".to_string())];
        let score = bank.score(&answers, &[]);
        assert_eq!((score.matched, score.total, score.refetches), (0, 2, 0));
    }

    #[test]
    fn empty_bank_fraction_is_none() {
        let bank = ProbeBank { probes: vec![] };
        assert_eq!(bank.score(&[], &[]).fraction(), None);
    }
}
