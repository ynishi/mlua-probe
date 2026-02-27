/// Result of a single test case.
#[derive(Debug, Clone)]
pub struct TestResult {
    /// Fully-qualified suite path (e.g. "math > addition").
    pub suite: String,
    /// Test name as passed to `it()`.
    pub name: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Error message if the test failed.
    pub error: Option<String>,
}

/// Aggregated results from a test run.
#[derive(Debug, Clone)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub total: usize,
    pub tests: Vec<TestResult>,
}
