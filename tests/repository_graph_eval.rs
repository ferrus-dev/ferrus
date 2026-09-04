//! Integration gates for graph retrieval quality, determinism, and navigation usefulness.

#[path = "support/repository_graph_eval.rs"]
mod evaluation;

use evaluation::{AutomationRecommendation, run_evaluation};

#[test]
fn rg2_navigation_corpus_meets_usefulness_gates() {
    let report = run_evaluation().unwrap();

    assert!(report.case_count >= 20);
    assert!(report.gates.all_passed);
    assert!(report.gates.exact_path_recall_at_1.passed);
    assert!(report.gates.exact_unique_symbol_recall_at_1.passed);
    assert!(report.gates.supported_discovery_recall_at_10.passed);
    assert!(report.gates.repeated_query_determinism.passed);
    assert!(report.gates.no_correctness_regression.passed);
    assert!(report.gates.navigation_context_reduction.passed);
    assert!(matches!(
        report.automation_recommendation,
        AutomationRecommendation::EligibleForStrongerGuidance
    ));

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["corpus_version"], "rg2.5-v1");
    assert!(json["distributions"]["graph_cold_latency_us"].is_array());
    assert!(json["distributions"]["graph_warm_latency_us"].is_array());
    assert!(json["distributions"]["graph_response_bytes"].is_array());
}
