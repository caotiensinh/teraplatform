//! The lint step: reads a configuration before it is planned and reports things that are probably
//! mistakes or bad habits. It never blocks a run and never changes anything; the findings are shown
//! next to the plan.
//!
//! Rules (stable names, so a finding can be searched for and later silenced):
//!
//! | rule | severity | what |
//! | --- | --- | --- |
//! | `parse-error` | warning | the text is not HCL this linter understands (the engine reports the real error) |
//! | `unused-variable` | warning | a `variable` that nothing refers to as `var.<name>` |
//! | `provider-version-missing` | warning | a provider in `required_providers` with no `version` |
//! | `provider-not-declared` | warning | resources of a provider that `required_providers` does not mention |
//! | `required-version-missing` | info | no `required_version` in a `terraform` block |
//! | `variable-without-description` | info | a `variable` with no `description` |
//! | `variable-without-type` | info | a `variable` with no `type` |
//! | `hardcoded-secret` | warning | a password, token or key written into the code as a literal |
//! | `interpolation-only` | info | `"${var.x}"` where `var.x` alone is meant (deprecated style) |
//!
//! Findings come in file and line order. When the configuration is several files joined by the
//! platform (`# file: name.tf` marker lines), the file and the line within it are reported.

use hcl::{Block, Expression};
use std::collections::BTreeSet;
use tf_core::LintFinding;

/// Attribute names that should never hold a literal.
const SECRET_NAMES: &[&str] = &["password", "secret", "secret_key", "secret_access_key", "access_key", "token", "api_key", "private_key", "client_secret", "auth_token"];

pub fn lint(config: &str) -> Vec<LintFinding> {
    let map = FileMap::new(config);
    let mut out: Vec<(usize, LintFinding)> = Vec::new(); // (line in the joined text, finding)
    let mut add = |line: Option<usize>, rule: &str, severity: &str, message: String| {
        let l = line.unwrap_or(0);
        let (file, rel) = if line.is_some() { map.locate(l) } else { (None, None) };
        out.push((l, LintFinding { rule: rule.into(), severity: severity.into(), message, file, line: rel }));
    };

    let body = match hcl::parse(config) {
        Ok(b) => b,
        Err(_) => {
            add(None, "parse-error", "warning", "this linter could not read the configuration as HCL; the engine will report the real error".into());
            return finish(out);
        }
    };

    // ---- variables
    let referenced = referenced_variables(config);
    for b in body.blocks().filter(|b| b.identifier() == "variable") {
        let Some(name) = b.labels().first().map(|l| l.as_str().to_string()) else { continue };
        let line = find_block_line(config, "variable", &name);
        if !referenced.contains(&name) {
            add(line, "unused-variable", "warning", format!("variable \"{name}\" is declared but never used (no var.{name})"));
        }
        if !has_attr(b, "description") {
            add(line, "variable-without-description", "info", format!("variable \"{name}\" has no description"));
        }
        if !has_attr(b, "type") {
            add(line, "variable-without-type", "info", format!("variable \"{name}\" has no type"));
        }
    }

    // ---- providers and versions
    let terraform: Vec<&Block> = body.blocks().filter(|b| b.identifier() == "terraform").collect();
    if !terraform.is_empty() && !terraform.iter().any(|b| has_attr(b, "required_version")) {
        add(find_line(config, "terraform"), "required-version-missing", "info", "the terraform block has no required_version".into());
    }
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for t in &terraform {
        for rp in t.body().blocks().filter(|b| b.identifier() == "required_providers") {
            for a in rp.body().attributes() {
                let name = a.key().to_string();
                declared.insert(name.clone());
                let pinned = match a.expr() {
                    Expression::Object(o) => o.iter().any(|(k, _)| object_key(k) == "version"),
                    _ => true, // a form this linter does not judge
                };
                if !pinned {
                    add(find_line(config, &name), "provider-version-missing", "warning", format!("provider \"{name}\" has no version constraint in required_providers"));
                }
            }
        }
    }
    let mut seen_missing: BTreeSet<String> = BTreeSet::new();
    for b in body.blocks().filter(|b| matches!(b.identifier(), "resource" | "data")) {
        let Some(ty) = b.labels().first().map(|l| l.as_str().to_string()) else { continue };
        let provider = ty.split('_').next().unwrap_or(&ty).to_string();
        if provider == "terraform" || declared.contains(&provider) || !seen_missing.insert(provider.clone()) {
            continue;
        }
        // Only worth saying when the configuration pins providers at all, or has no pin anywhere.
        let line = find_block_line(config, b.identifier(), &ty);
        add(line, "provider-not-declared", "warning", format!("{} \"{ty}\" uses provider \"{provider}\", which required_providers does not declare", b.identifier()));
    }

    // ---- secrets written into the code
    for b in body.blocks().filter(|b| matches!(b.identifier(), "resource" | "provider" | "module" | "data")) {
        let label = b.labels().iter().map(|l| l.as_str()).collect::<Vec<_>>().join(".");
        for a in b.body().attributes() {
            let key = a.key().to_string();
            if !SECRET_NAMES.contains(&key.as_str()) {
                continue;
            }
            if let Expression::String(s) = a.expr() {
                if !s.is_empty() {
                    let line = find_attr_line(config, &key);
                    add(line, "hardcoded-secret", "warning", format!("{} \"{label}\" sets {key} to a literal; pass it as a sensitive variable or from the environment", b.identifier()));
                }
            }
        }
    }

    // ---- "${var.x}" on its own
    for (i, line) in config.lines().enumerate() {
        if let Some(expr) = lone_interpolation(line) {
            add(Some(i + 1), "interpolation-only", "info", format!("\"${{{expr}}}\" can be written as {expr} (quoting a lone reference is the pre-0.12 style)"));
        }
    }

    finish(out)
}

fn finish(mut out: Vec<(usize, LintFinding)>) -> Vec<LintFinding> {
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.rule.cmp(&b.1.rule)).then_with(|| a.1.message.cmp(&b.1.message)));
    out.dedup_by(|a, b| a.1 == b.1);
    out.into_iter().map(|(_, f)| f).collect()
}

fn has_attr(b: &Block, name: &str) -> bool {
    b.body().attributes().any(|a| a.key() == name)
}

fn object_key(k: &hcl::expr::ObjectKey) -> String {
    match k {
        hcl::expr::ObjectKey::Identifier(i) => i.to_string(),
        hcl::expr::ObjectKey::Expression(Expression::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Names that appear as `var.<name>` anywhere in the text. A text scan, not a parse, on purpose: a
/// reference inside a string template or a comment still counts as a use, so the rule errs towards
/// staying quiet.
fn referenced_variables(config: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let b = config.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        if &b[i..i + 4] == b"var." && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'-')) {
            let start = i + 4;
            let mut end = start;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'-') {
                end += 1;
            }
            if end > start {
                out.insert(config[start..end].to_string());
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// The expression of a line that is exactly one quoted interpolation: `x = "${var.name}"`.
fn lone_interpolation(line: &str) -> Option<String> {
    let t = line.trim();
    let (_, rhs) = t.split_once('=')?;
    let rhs = rhs.trim();
    let inner = rhs.strip_prefix("\"${")?.strip_suffix("}\"")?;
    if inner.is_empty() || inner.contains("${") || inner.contains('"') || inner.contains('}') {
        return None;
    }
    Some(inner.trim().to_string())
}

/// 1-based line of the first line that starts (after spaces) with `word`.
fn find_line(config: &str, word: &str) -> Option<usize> {
    config.lines().position(|l| l.trim_start().starts_with(word)).map(|i| i + 1)
}

/// 1-based line of `<kind> "<label>"`.
fn find_block_line(config: &str, kind: &str, label: &str) -> Option<usize> {
    let needle = format!("\"{label}\"");
    config.lines().position(|l| {
        let t = l.trim_start();
        t.starts_with(kind) && t[kind.len()..].trim_start().starts_with(&needle)
    }).map(|i| i + 1)
}

/// 1-based line of the first `<key> = "..."` assignment.
fn find_attr_line(config: &str, key: &str) -> Option<usize> {
    config.lines().position(|l| {
        let t = l.trim_start();
        t.starts_with(key) && t[key.len()..].trim_start().starts_with('=')
    }).map(|i| i + 1)
}

/// Maps a line of the joined configuration back to its file, using the `# file: name` markers the
/// platform writes when it joins an archive's files.
struct FileMap {
    marks: Vec<(usize, String)>, // (line of the marker, file name)
}

impl FileMap {
    fn new(config: &str) -> Self {
        let marks = config
            .lines()
            .enumerate()
            .filter_map(|(i, l)| l.strip_prefix("# file: ").map(|n| (i + 1, n.trim().to_string())))
            .collect();
        FileMap { marks }
    }

    /// (file, line within the file) for a 1-based line; plain text has no file.
    fn locate(&self, line: usize) -> (Option<String>, Option<u32>) {
        if self.marks.is_empty() {
            return (None, Some(line as u32));
        }
        match self.marks.iter().rev().find(|(m, _)| *m <= line) {
            Some((m, name)) => (Some(name.clone()), Some((line - m) as u32)),
            None => (None, Some(line as u32)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(cfg: &str) -> Vec<(String, String)> {
        lint(cfg).into_iter().map(|f| (f.rule, f.severity)).collect()
    }
    fn has(cfg: &str, rule: &str) -> bool {
        lint(cfg).iter().any(|f| f.rule == rule)
    }

    const CLEAN: &str = r#"
terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "bucket" {
  type        = string
  description = "name of the bucket"
}

resource "aws_s3_bucket" "logs" {
  bucket = var.bucket
}
"#;

    #[test]
    fn a_tidy_configuration_has_no_findings() {
        assert_eq!(lint(CLEAN), vec![], "{:?}", lint(CLEAN));
    }

    #[test]
    fn an_empty_text_has_no_findings() {
        assert!(lint("").is_empty());
        assert!(lint("# only a comment\n").is_empty());
    }

    #[test]
    fn unused_variables_are_found_and_used_ones_are_not() {
        let cfg = format!("{CLEAN}\nvariable \"spare\" {{\n  type = string\n  description = \"x\"\n}}\n");
        let f = lint(&cfg);
        let u: Vec<_> = f.iter().filter(|f| f.rule == "unused-variable").collect();
        assert_eq!(u.len(), 1);
        assert!(u[0].message.contains("\"spare\"") && u[0].severity == "warning");
        assert_eq!(u[0].line, Some(cfg.lines().position(|l| l.starts_with("variable \"spare\"")).unwrap() as u32 + 1));
        // a use inside a template string counts
        let t = "variable \"n\" {\n  type = string\n  description = \"d\"\n}\nresource \"null_resource\" \"a\" {\n  triggers = { x = \"id-${var.n}\" }\n}\n";
        assert!(!has(t, "unused-variable"));
        // a longer name does not count as a use of a shorter one
        let c = "variable \"a\" {\n  type = string\n  description = \"d\"\n}\noutput \"o\" {\n  value = var.ab\n}\n";
        assert!(has(c, "unused-variable"));
    }

    #[test]
    fn variables_without_description_or_type_are_info() {
        let cfg = "variable \"x\" {\n}\noutput \"o\" {\n  value = var.x\n}\n";
        let r = rules(cfg);
        assert!(r.contains(&("variable-without-description".into(), "info".into())));
        assert!(r.contains(&("variable-without-type".into(), "info".into())));
        assert!(!has(cfg, "unused-variable"));
    }

    #[test]
    fn provider_versions_must_be_pinned() {
        let cfg = "terraform {\n  required_version = \">= 1.0\"\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n    }\n    random = {\n      source  = \"hashicorp/random\"\n      version = \"3.6.0\"\n    }\n  }\n}\n";
        let f: Vec<_> = lint(cfg).into_iter().filter(|f| f.rule == "provider-version-missing").collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("\"aws\""));
    }

    #[test]
    fn resources_of_an_undeclared_provider_are_found_once_per_provider() {
        let cfg = "terraform {\n  required_version = \">= 1.0\"\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n      version = \"~> 5\"\n    }\n  }\n}\nresource \"aws_s3_bucket\" \"a\" {\n}\nresource \"google_storage_bucket\" \"b\" {\n}\nresource \"google_compute_instance\" \"c\" {\n}\nresource \"terraform_data\" \"d\" {\n}\n";
        let f: Vec<_> = lint(cfg).into_iter().filter(|f| f.rule == "provider-not-declared").collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("google"), "built-in terraform_data and the declared aws are fine: {f:?}");
    }

    #[test]
    fn a_missing_required_version_is_info_and_only_when_a_terraform_block_exists() {
        assert!(has("terraform {\n  required_providers {\n  }\n}\n", "required-version-missing"));
        assert!(!has("resource \"null_resource\" \"a\" {\n}\n", "required-version-missing"), "no terraform block, nothing to say");
    }

    #[test]
    fn literal_secrets_are_flagged_but_references_and_empty_values_are_not() {
        let bad = "resource \"aws_db_instance\" \"d\" {\n  username = \"admin\"\n  password = \"hunter2\"\n}\nprovider \"aws\" {\n  access_key = \"AKIA...\"\n}\n";
        let f: Vec<_> = lint(bad).into_iter().filter(|f| f.rule == "hardcoded-secret").collect();
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].message.contains("password") && f[0].line == Some(3));
        assert!(!f.iter().any(|f| f.message.contains("hunter2")), "the finding never repeats the secret");
        let ok = "resource \"aws_db_instance\" \"d\" {\n  password = var.db_password\n  token = \"\"\n}\nvariable \"db_password\" {\n  type = string\n  description = \"d\"\n  sensitive = true\n}\n";
        assert!(!has(ok, "hardcoded-secret"));
        // a variable whose default is a literal is not a resource attribute
        assert!(!has("variable \"password\" {\n  default = \"x\"\n}\n", "hardcoded-secret"));
    }

    #[test]
    fn a_lone_interpolation_is_info_and_a_mixed_template_is_not() {
        let cfg = "resource \"null_resource\" \"a\" {\n  triggers = {\n    a = \"${var.x}\"\n    b = \"id-${var.x}\"\n    c = \"${var.x}-${var.y}\"\n  }\n}\nvariable \"x\" {\n  type = string\n  description = \"d\"\n}\nvariable \"y\" {\n  type = string\n  description = \"d\"\n}\n";
        let f: Vec<_> = lint(cfg).into_iter().filter(|f| f.rule == "interpolation-only").collect();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].line, Some(3));
        assert!(f[0].message.contains("var.x"));
    }

    #[test]
    fn text_that_is_not_hcl_gives_one_warning_and_nothing_else() {
        let f = lint("resource \"a\" {{{ not valid");
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].rule.as_str(), f[0].severity.as_str()), ("parse-error", "warning"));
    }

    #[test]
    fn findings_in_a_joined_archive_name_their_file_and_line() {
        let cfg = "# file: a.tf\nvariable \"one\" {\n  type = string\n  description = \"d\"\n}\n# file: b.tf\nvariable \"two\" {\n  type = string\n  description = \"d\"\n}\n";
        let f = lint(cfg);
        let one = f.iter().find(|f| f.message.contains("\"one\"")).unwrap();
        let two = f.iter().find(|f| f.message.contains("\"two\"")).unwrap();
        assert_eq!((one.file.as_deref(), one.line), (Some("a.tf"), Some(1)));
        assert_eq!((two.file.as_deref(), two.line), (Some("b.tf"), Some(1)));
    }

    #[test]
    fn findings_are_in_line_order_and_stable() {
        let cfg = "variable \"b\" {\n}\nvariable \"a\" {\n}\n";
        let first = lint(cfg);
        assert_eq!(first, lint(cfg));
        let lines: Vec<_> = first.iter().map(|f| f.line.unwrap()).collect();
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
    }

    #[test]
    fn linting_never_panics_on_odd_input() {
        for s in ["\u{0}", "}}}}", "variable", "variable \"", "terraform {", "a = \"${", "resource \"x\" \"y\" {\n  password = \n}", &"{".repeat(2000)] {
            let _ = lint(s);
        }
    }
}
