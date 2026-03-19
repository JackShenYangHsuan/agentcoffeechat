use regex::Regex;

// ---------------------------------------------------------------------------
// SanitizeResult
// ---------------------------------------------------------------------------

/// The output of a single sanitization stage (or the full pipeline).
#[derive(Debug, Clone)]
pub struct SanitizeResult {
    pub text: String,
    pub redaction_count: usize,
    pub blocked: bool,
    pub block_reason: Option<String>,
}

impl SanitizeResult {
    /// Convenience constructor for the common pass-through case.
    pub fn pass(text: String) -> Self {
        Self {
            text,
            redaction_count: 0,
            blocked: false,
            block_reason: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SanitizationStage trait
// ---------------------------------------------------------------------------

/// A single stage in the sanitization pipeline.
pub trait SanitizationStage {
    fn sanitize(&self, text: &str) -> SanitizeResult;
}

// ===========================================================================
// Stage 1 — ExclusionStage (path reference stripping)
// ===========================================================================

/// Strips references to file paths that match any of the configured patterns.
/// Uses simple string matching against path segments and extensions.
pub struct ExclusionStage {
    patterns: Vec<String>,
    /// Pre-compiled regex for matching file-path-like tokens in text.
    path_re: Regex,
}

impl Default for ExclusionStage {
    fn default() -> Self {
        Self::new(vec![
            ".env".into(),
            "*.pem".into(),
            "*.key".into(),
            "*.p12".into(),
            "*.pfx".into(),
            "*.jks".into(),
            "node_modules/".into(),
            ".git/".into(),
            "id_rsa".into(),
            "id_dsa".into(),
            "id_ecdsa".into(),
            "id_ed25519".into(),
            ".ssh/".into(),
            "credentials".into(),
            "*.secret".into(),
            ".aws/".into(),
        ])
    }
}

impl ExclusionStage {
    pub fn new(patterns: Vec<String>) -> Self {
        let path_re =
            Regex::new(r#"(?:^|[\s"'`])(/?\S+(?:/\S+)+|\.[\w]+)"#).unwrap();
        Self { patterns, path_re }
    }

    /// Simple pattern matching against a path string.
    ///
    /// Supported pattern forms:
    /// - `*.ext`         — matches any path ending with `.ext`
    /// - `dirname/`      — matches any path containing `/dirname/` or starting
    ///                      with `dirname/`
    /// - `.filename`     — matches any path whose final component is `.filename`
    /// - `filename`      — matches any path whose final component equals
    ///                      `filename`
    fn pattern_matches(pattern: &str, path: &str) -> bool {
        let path = path.trim_start_matches('/');

        if let Some(ext) = pattern.strip_prefix("*.") {
            // Glob extension match: *.pem, *.key, etc.
            return path.ends_with(&format!(".{ext}"));
        }

        if pattern.ends_with('/') {
            // Directory pattern: node_modules/, .git/, .ssh/, .aws/
            let dir = pattern.trim_end_matches('/');
            return path.starts_with(&format!("{dir}/"))
                || path.contains(&format!("/{dir}/"))
                || path == dir;
        }

        // Exact filename match against the last path component or the whole
        // path (for things like `.env`, `id_rsa`, `credentials`).
        let filename = path.rsplit('/').next().unwrap_or(path);
        filename == pattern || path == pattern
    }
}

impl SanitizationStage for ExclusionStage {
    fn sanitize(&self, text: &str) -> SanitizeResult {
        if self.patterns.is_empty() {
            return SanitizeResult::pass(text.to_string());
        }

        let mut result = text.to_string();
        let mut redaction_count: usize = 0;

        // Collect matches first, then replace in reverse order so byte
        // offsets remain valid.
        let matches: Vec<(String, usize, usize)> = self.path_re
            .captures_iter(text)
            .filter_map(|cap| {
                let m = cap.get(1)?;
                Some((m.as_str().to_string(), m.start(), m.end()))
            })
            .collect();

        for (path, start, end) in matches.iter().rev() {
            let excluded = self
                .patterns
                .iter()
                .any(|pat| Self::pattern_matches(pat, path));
            if excluded {
                result.replace_range(*start..*end, "[EXCLUDED_PATH]");
                redaction_count += 1;
            }
        }

        SanitizeResult {
            text: result,
            redaction_count,
            blocked: false,
            block_reason: None,
        }
    }
}

// ===========================================================================
// Stage 2 — EnvVarStripStage
// ===========================================================================

/// Detects environment variable assignments and references, replacing matched
/// content with `[REDACTED]`.
pub struct EnvVarStripStage {
    patterns: Vec<Regex>,
}

impl Default for EnvVarStripStage {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvVarStripStage {
    pub fn new() -> Self {
        let raw = vec![
            // export FOO=bar  (must come before the generic FOO=bar pattern)
            r"export\s+[A-Z_]+=\S+",
            // FOO=bar
            r"\b[A-Z_]{2,}=\S+",
            // process.env.FOO
            r"process\.env\.[A-Z_]+",
            // os.environ["FOO"]
            r#"os\.environ\["[^"]+"\]"#,
            // env::var("FOO")
            r#"env::var\("[^"]+"\)"#,
        ];

        let patterns = raw
            .into_iter()
            .map(|p| Regex::new(p).expect("invalid env-var regex"))
            .collect();

        Self { patterns }
    }
}

impl SanitizationStage for EnvVarStripStage {
    fn sanitize(&self, text: &str) -> SanitizeResult {
        let mut result = text.to_string();
        let mut redaction_count: usize = 0;

        for re in &self.patterns {
            let before = result.clone();
            let count = re.find_iter(&before).count();
            let after = re.replace_all(&result, "[REDACTED]");
            if after != before {
                redaction_count += count;
                result = after.into_owned();
            }
        }

        SanitizeResult {
            text: result,
            redaction_count,
            blocked: false,
            block_reason: None,
        }
    }
}

// ===========================================================================
// Stage 3 — RegexRedactionStage
// ===========================================================================

/// Redacts a broad set of secret patterns: API keys, tokens, private keys,
/// connection strings, and more. All matches are replaced with `[REDACTED]`.
pub struct RegexRedactionStage {
    rules: Vec<Regex>,
}

impl Default for RegexRedactionStage {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexRedactionStage {
    pub fn new() -> Self {
        let raw_patterns: Vec<&str> = vec![
            // AWS access key IDs
            r"AKIA[0-9A-Z]{16}",
            // Generic token / key / secret / password / credential assignments
            r#"(?i)(token|key|secret|password|credential)\s*[=:]\s*['"]?\S{8,}"#,
            // Bearer tokens
            r"Bearer\s+[A-Za-z0-9\-._~+/]+=*",
            // PEM private key headers
            r"-----BEGIN\s+(RSA\s+|EC\s+|OPENSSH\s+)?PRIVATE KEY-----",
            // URLs with embedded passwords (scheme://user:pass@host)
            r"://[^:]+:[^@]+@",
            // Connection strings
            r"(?i)(postgres|mysql|mongodb|redis)://\S+",
            // IP:port patterns
            r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}:\d+",
            // GitHub / GitLab tokens
            r"(ghp_|gho_|ghu_|ghs_|ghr_|glpat-)[A-Za-z0-9_]+",
            // Slack tokens
            r"xox[bpras]-[A-Za-z0-9\-]+",
            // JWT tokens
            r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        ];

        let rules = raw_patterns
            .into_iter()
            .map(|p| Regex::new(p).unwrap_or_else(|e| panic!("bad regex `{p}`: {e}")))
            .collect();

        Self { rules }
    }
}

impl SanitizationStage for RegexRedactionStage {
    fn sanitize(&self, text: &str) -> SanitizeResult {
        let mut result = text.to_string();
        let mut redaction_count: usize = 0;

        for re in &self.rules {
            let before = result.clone();
            let count = re.find_iter(&before).count();
            let after = re.replace_all(&result, "[REDACTED]");
            if after != before {
                redaction_count += count;
                result = after.into_owned();
            }
        }

        SanitizeResult {
            text: result,
            redaction_count,
            blocked: false,
            block_reason: None,
        }
    }
}

// ===========================================================================
// Stage 4 — AutoScanStage
// ===========================================================================

/// Final high-confidence gate. If extremely sensitive material is still
/// present after earlier stages, this stage sets `blocked = true`.
pub struct AutoScanStage {
    block_patterns: Vec<(Regex, &'static str)>,
}

impl Default for AutoScanStage {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoScanStage {
    pub fn new() -> Self {
        let block_patterns = vec![
            (
                Regex::new(
                    r"-----BEGIN\s+(RSA\s+|EC\s+|OPENSSH\s+)?PRIVATE KEY-----",
                )
                .unwrap(),
                "Private key detected after sanitization",
            ),
            (
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                "AWS access key detected after sanitization",
            ),
        ];
        Self { block_patterns }
    }
}

impl SanitizationStage for AutoScanStage {
    fn sanitize(&self, text: &str) -> SanitizeResult {
        for (re, reason) in &self.block_patterns {
            if re.is_match(text) {
                return SanitizeResult {
                    text: text.to_string(),
                    redaction_count: 0,
                    blocked: true,
                    block_reason: Some((*reason).to_string()),
                };
            }
        }
        SanitizeResult::pass(text.to_string())
    }
}

// ===========================================================================
// SanitizationPipeline
// ===========================================================================

/// Chains all four sanitization stages and runs them sequentially.
pub struct SanitizationPipeline {
    stages: Vec<Box<dyn SanitizationStage + Send + Sync>>,
}

impl Default for SanitizationPipeline {
    /// Creates the full four-stage pipeline with all default patterns.
    fn default() -> Self {
        let stages: Vec<Box<dyn SanitizationStage + Send + Sync>> = vec![
            Box::new(ExclusionStage::default()),
            Box::new(EnvVarStripStage::new()),
            Box::new(RegexRedactionStage::new()),
            Box::new(AutoScanStage::new()),
        ];
        Self { stages }
    }
}

impl SanitizationPipeline {
    /// Build the pipeline with custom exclusion patterns (stages 2-4 use
    /// defaults).
    pub fn new(exclusion_patterns: Vec<String>) -> Self {
        let stages: Vec<Box<dyn SanitizationStage + Send + Sync>> = vec![
            Box::new(ExclusionStage::new(exclusion_patterns)),
            Box::new(EnvVarStripStage::new()),
            Box::new(RegexRedactionStage::new()),
            Box::new(AutoScanStage::new()),
        ];
        Self { stages }
    }

    /// Run every stage in order. If any stage blocks, the pipeline stops
    /// immediately and returns the blocked result with accumulated redactions.
    pub fn run(&self, text: &str) -> SanitizeResult {
        let mut current_text = text.to_string();
        let mut total_redactions: usize = 0;

        for stage in &self.stages {
            let res = stage.sanitize(&current_text);
            total_redactions += res.redaction_count;
            if res.blocked {
                return SanitizeResult {
                    text: res.text,
                    redaction_count: total_redactions,
                    blocked: true,
                    block_reason: res.block_reason,
                };
            }
            current_text = res.text;
        }

        SanitizeResult {
            text: current_text,
            redaction_count: total_redactions,
            blocked: false,
            block_reason: None,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Stage 1: ExclusionStage -------------------------------------------

    #[test]
    fn exclusion_strips_env_path() {
        let stage = ExclusionStage::default();
        let input = "Check the file /app/config/.env for details";
        let res = stage.sanitize(input);
        assert!(
            !res.text.contains("/app/config/.env"),
            "Should strip .env path: {}",
            res.text
        );
        assert!(res.redaction_count >= 1);
    }

    #[test]
    fn exclusion_strips_pem_path() {
        let stage = ExclusionStage::default();
        let input = "Load /etc/ssl/server.pem for TLS";
        let res = stage.sanitize(input);
        assert!(
            !res.text.contains("/etc/ssl/server.pem"),
            "Should strip .pem path: {}",
            res.text
        );
    }

    #[test]
    fn exclusion_strips_key_path() {
        let stage = ExclusionStage::default();
        let input = "Use /home/user/.ssh/private.key here";
        let res = stage.sanitize(input);
        assert!(
            !res.text.contains("private.key"),
            "Should strip .key path: {}",
            res.text
        );
    }

    #[test]
    fn exclusion_strips_node_modules_path() {
        let stage = ExclusionStage::default();
        let input = "Imported from /project/node_modules/foo/index.js";
        let res = stage.sanitize(input);
        assert!(
            !res.text.contains("node_modules"),
            "Should strip node_modules path: {}",
            res.text
        );
    }

    #[test]
    fn exclusion_ignores_non_matching_paths() {
        let stage = ExclusionStage::default();
        let input = "The file /app/src/main.rs is fine";
        let res = stage.sanitize(input);
        assert!(res.text.contains("/app/src/main.rs"));
        assert_eq!(res.redaction_count, 0);
    }

    #[test]
    fn exclusion_empty_patterns_is_noop() {
        let stage = ExclusionStage::new(vec![]);
        let res = stage.sanitize("nothing to do /foo/bar.env");
        assert_eq!(res.text, "nothing to do /foo/bar.env");
        assert_eq!(res.redaction_count, 0);
    }

    #[test]
    fn exclusion_custom_patterns() {
        let stage = ExclusionStage::new(vec!["*.env".into(), "secrets/".into()]);
        let input = "Check /app/.env and also /app/secrets/keys.json for details";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("/app/.env"), "Got: {}", res.text);
        assert!(
            !res.text.contains("/app/secrets/keys.json"),
            "Got: {}",
            res.text
        );
        assert!(res.redaction_count >= 2);
    }

    // -- Stage 2: EnvVarStripStage -----------------------------------------

    #[test]
    fn env_var_foo_equals_bar() {
        let stage = EnvVarStripStage::new();
        let res = stage.sanitize("DATABASE_URL=postgres://user:pass@host/db");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("postgres://"));
        assert!(res.redaction_count >= 1);
    }

    #[test]
    fn env_var_export_assignment() {
        let stage = EnvVarStripStage::new();
        let res = stage.sanitize("export SECRET_KEY=my-secret-value");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("my-secret-value"));
        assert!(res.redaction_count >= 1);
    }

    #[test]
    fn env_var_process_env() {
        let stage = EnvVarStripStage::new();
        let res = stage.sanitize("const key = process.env.API_KEY;");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("process.env.API_KEY"));
    }

    #[test]
    fn env_var_python_environ() {
        let stage = EnvVarStripStage::new();
        let res = stage.sanitize("val = os.environ[\"DB_PASSWORD\"]");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("os.environ"));
    }

    #[test]
    fn env_var_rust_env_var() {
        let stage = EnvVarStripStage::new();
        let res = stage.sanitize("let key = env::var(\"SECRET_TOKEN\");");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("env::var"));
    }

    // -- Stage 3: RegexRedactionStage --------------------------------------

    #[test]
    fn regex_redacts_aws_key() {
        let stage = RegexRedactionStage::new();
        let res = stage.sanitize("key: AKIAIOSFODNN7EXAMPLE ");
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn regex_redacts_generic_token() {
        let stage = RegexRedactionStage::new();
        let input = "token = sk-abc123def456ghi789jkl012";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("sk-abc123"));
    }

    #[test]
    fn regex_redacts_bearer_token() {
        let stage = RegexRedactionStage::new();
        let input = "Authorization: Bearer abc123def456.xyz789";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("abc123def456"));
    }

    #[test]
    fn regex_redacts_private_key_header() {
        let stage = RegexRedactionStage::new();
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIBogIB...";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("-----BEGIN RSA PRIVATE KEY-----"));
    }

    #[test]
    fn regex_redacts_private_key_ec() {
        let stage = RegexRedactionStage::new();
        let input = "-----BEGIN EC PRIVATE KEY-----\ndata";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_private_key_openssh() {
        let stage = RegexRedactionStage::new();
        let input = "-----BEGIN OPENSSH PRIVATE KEY-----\ndata";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_url_with_password() {
        let stage = RegexRedactionStage::new();
        let input = "https://admin:supersecret@example.com";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("supersecret"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_connection_string_postgres() {
        let stage = RegexRedactionStage::new();
        let input = "DATABASE=postgres://user:pw@host:5432/prod";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("postgres://"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_connection_string_mongodb() {
        let stage = RegexRedactionStage::new();
        let input = "MONGO=mongodb://user:pw@mongo:27017/prod";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("mongodb://"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_connection_string_mysql() {
        let stage = RegexRedactionStage::new();
        let input = "mysql://root:pass@localhost:3306/db";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("mysql://"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_connection_string_redis() {
        let stage = RegexRedactionStage::new();
        let input = "redis://default:pass@redis:6379";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("redis://"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_ip_port() {
        let stage = RegexRedactionStage::new();
        let input = "Connect to 10.0.0.1:8080 for access";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("10.0.0.1:8080"));
    }

    #[test]
    fn regex_redacts_github_token_ghp() {
        let stage = RegexRedactionStage::new();
        let input = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("ghp_"));
    }

    #[test]
    fn regex_redacts_github_token_gho() {
        let stage = RegexRedactionStage::new();
        let input = "token: gho_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("gho_"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_github_token_ghs() {
        let stage = RegexRedactionStage::new();
        let input = "ghs_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("ghs_"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_gitlab_token() {
        let stage = RegexRedactionStage::new();
        let input = "token: glpat-abcdefghij1234567890";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("glpat-"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_slack_token() {
        let stage = RegexRedactionStage::new();
        let input = "SLACK_TOKEN=xoxb-FAKE0SLACK-TESTTOKEN1";
        let res = stage.sanitize(input);
        assert!(!res.text.contains("xoxb-"), "Got: {}", res.text);
    }

    #[test]
    fn regex_redacts_jwt() {
        let stage = RegexRedactionStage::new();
        let input = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let res = stage.sanitize(input);
        assert!(res.text.contains("[REDACTED]"), "Got: {}", res.text);
        assert!(!res.text.contains("eyJhbG"));
    }

    // -- Stage 4: AutoScanStage --------------------------------------------

    #[test]
    fn autoscan_blocks_private_key() {
        let stage = AutoScanStage::new();
        let input = "-----BEGIN PRIVATE KEY-----\ndata\n-----END PRIVATE KEY-----";
        let res = stage.sanitize(input);
        assert!(res.blocked);
        assert!(res.block_reason.unwrap().contains("Private key"));
    }

    #[test]
    fn autoscan_blocks_rsa_private_key() {
        let stage = AutoScanStage::new();
        let input =
            "-----BEGIN RSA PRIVATE KEY-----\ndata\n-----END RSA PRIVATE KEY-----";
        let res = stage.sanitize(input);
        assert!(res.blocked);
    }

    #[test]
    fn autoscan_blocks_aws_key() {
        let stage = AutoScanStage::new();
        let input = "my key is AKIAIOSFODNN7EXAMPLE ok";
        let res = stage.sanitize(input);
        assert!(res.blocked);
        assert!(res.block_reason.unwrap().contains("AWS"));
    }

    #[test]
    fn autoscan_passes_clean_text() {
        let stage = AutoScanStage::new();
        let res = stage.sanitize("Just a normal message about coding.");
        assert!(!res.blocked);
        assert!(res.block_reason.is_none());
    }

    // -- Full pipeline -----------------------------------------------------

    #[test]
    fn pipeline_clean_text_passes() {
        let pipeline = SanitizationPipeline::default();
        let res = pipeline.run("Hello, let's discuss our Rust project.");
        assert!(!res.blocked);
        assert_eq!(res.redaction_count, 0);
        assert!(res.text.contains("Rust project"));
    }

    #[test]
    fn pipeline_clean_text_unchanged() {
        let pipeline = SanitizationPipeline::default();
        let input = "This is perfectly clean text with no secrets at all.";
        let res = pipeline.run(input);
        assert!(!res.blocked);
        assert_eq!(res.redaction_count, 0);
        assert_eq!(res.text, input);
    }

    #[test]
    fn pipeline_redacts_env_and_github_token() {
        let pipeline = SanitizationPipeline::default();
        let input = concat!(
            "DATABASE_URL=postgres://admin:pw@host/db ",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"
        );
        let res = pipeline.run(input);
        assert!(!res.blocked);
        assert!(res.redaction_count > 0);
        assert!(!res.text.contains("admin:pw"), "Got: {}", res.text);
        assert!(!res.text.contains("ghp_ABCDEF"), "Got: {}", res.text);
    }

    #[test]
    fn pipeline_stage3_redacts_private_key_before_stage4() {
        let pipeline = SanitizationPipeline::default();
        let input =
            "Here:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBogIBAAJ\n-----END RSA PRIVATE KEY-----";
        let res = pipeline.run(input);
        // Stage 3 should have replaced the key header, so Stage 4 won't block.
        assert!(
            !res.text.contains("-----BEGIN RSA PRIVATE KEY-----")
                || res.blocked,
            "Key should be redacted or blocked: {}",
            res.text
        );
    }

    #[test]
    fn pipeline_blocks_private_key_if_it_survives() {
        // Feed raw key directly to Stage 4 to verify blocking behaviour.
        let stage = AutoScanStage::new();
        let res = stage.sanitize(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIBogIBAAJ\n-----END RSA PRIVATE KEY-----",
        );
        assert!(res.blocked);
    }

    #[test]
    fn pipeline_exclusion_and_secrets_combined() {
        let pipeline = SanitizationPipeline::new(vec!["*.env".into()]);
        let input = "File /config/.env has SECRET_KEY=hunter2_super_secret";
        let res = pipeline.run(input);
        assert!(!res.blocked);
        assert!(
            !res.text.contains("/config/.env"),
            "Excluded path should be gone: {}",
            res.text
        );
        assert!(
            !res.text.contains("hunter2"),
            "Secret should be redacted: {}",
            res.text
        );
        assert!(res.redaction_count >= 2);
    }

    #[test]
    fn pipeline_multiple_planted_secrets() {
        let pipeline = SanitizationPipeline::default();
        let input = concat!(
            "export API_KEY=sk-live-abc123def456ghi789\n",
            "SLACK=xoxb-FAKE0SLACK-TESTTOKEN1\n",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            ".eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            ".dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U\n",
            "server at 10.0.0.5:3000\n",
            "db: postgres://admin:secret@db.host:5432/mydb\n",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij\n",
        );
        let res = pipeline.run(input);
        assert!(!res.blocked);
        assert!(
            res.redaction_count >= 4,
            "Expected >= 4 redactions, got {}",
            res.redaction_count
        );
        assert!(!res.text.contains("sk-live-"), "Got: {}", res.text);
        assert!(!res.text.contains("xoxb-"), "Got: {}", res.text);
        assert!(!res.text.contains("eyJhbG"), "Got: {}", res.text);
        assert!(
            !res.text.contains("10.0.0.5:3000"),
            "Got: {}",
            res.text
        );
        assert!(
            !res.text.contains("postgres://"),
            "Got: {}",
            res.text
        );
        assert!(!res.text.contains("ghp_"), "Got: {}", res.text);
    }

    #[test]
    fn pipeline_default_constructor() {
        // Ensure SanitizationPipeline::default() works and includes the
        // default exclusion patterns.
        let pipeline = SanitizationPipeline::default();
        let input =
            "Load /etc/ssl/cert.pem and also process.env.SECRET_KEY";
        let res = pipeline.run(input);
        assert!(!res.blocked);
        assert!(res.redaction_count >= 1);
        assert!(
            !res.text.contains("process.env.SECRET_KEY"),
            "Got: {}",
            res.text
        );
    }
}
