use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ast_grep_config::{RuleCollection, RuleConfig, Severity};
use ast_grep_core::{AstGrep, Matcher, NodeMatch};
use clap::Args;
use ignore::WalkParallel;

use crate::config::{find_rules, read_rule_file, IgnoreFile, NoIgnore};
use crate::error::ErrorContext as EC;
use crate::print::{
  ColorArg, ColoredPrinter, Diff, InteractivePrinter, JSONPrinter, Printer, ReportStyle, SimpleFile,
};
use crate::utils::filter_file_interactive;
use crate::utils::{run_worker, Items, Worker};
use ast_grep_language::SupportLang;

#[derive(Args)]
pub struct ScanArg {
  /// Path to ast-grep root config, default is sgconfig.yml.
  #[clap(short, long)]
  config: Option<PathBuf>,

  /// Scan the codebase with one specified rule, without project config setup.
  #[clap(short, long, conflicts_with = "config")]
  rule: Option<PathBuf>,

  /// Start interactive edit session. Code rewrite only happens inside a session.
  #[clap(short, long, conflicts_with = "json")]
  interactive: bool,

  /// Controls output color.
  #[clap(long, default_value = "auto")]
  color: ColorArg,

  #[clap(long, default_value = "rich")]
  report_style: ReportStyle,

  /// Output matches in structured JSON text. This is useful for tools like jq.
  /// Conflicts with color and report-style.
  #[clap(long, conflicts_with = "color", conflicts_with = "report_style")]
  json: bool,

  /// Apply all rewrite without confirmation if true.
  #[clap(long)]
  accept_all: bool,

  /// The paths to search. You can provide multiple paths separated by spaces.
  #[clap(value_parser, default_value = ".")]
  paths: Vec<PathBuf>,

  /// Do not respect ignore files. You can suppress multiple ignore files by passing `no-ignore` multiple times.
  #[clap(long, action = clap::ArgAction::Append)]
  no_ignore: Vec<IgnoreFile>,
}

pub fn run_with_config(arg: ScanArg) -> Result<()> {
  if arg.json {
    let worker = ScanWithConfig::try_new(arg, JSONPrinter::stdout())?;
    return run_worker(worker);
  }
  let printer = ColoredPrinter::stdout(arg.color).style(arg.report_style);
  let interactive = arg.interactive || arg.accept_all;
  if interactive {
    let printer = InteractivePrinter::new(printer).accept_all(arg.accept_all);
    let worker = ScanWithConfig::try_new(arg, printer)?;
    run_worker(worker)
  } else {
    let worker = ScanWithConfig::try_new(arg, printer)?;
    run_worker(worker)
  }
}

struct ScanWithConfig<Printer> {
  arg: ScanArg,
  printer: Printer,
  configs: RuleCollection<SupportLang>,
}
impl<P: Printer> ScanWithConfig<P> {
  fn try_new(mut arg: ScanArg, printer: P) -> Result<Self> {
    let configs = if let Some(path) = &arg.rule {
      let rules = read_rule_file(path, None)?;
      RuleCollection::try_new(rules).context(EC::GlobPattern)?
    } else {
      find_rules(arg.config.take())?
    };
    Ok(Self {
      arg,
      printer,
      configs,
    })
  }
}

impl<P: Printer + Sync> Worker for ScanWithConfig<P> {
  type Item = (PathBuf, AstGrep<SupportLang>);
  fn build_walk(&self) -> WalkParallel {
    let arg = &self.arg;
    let threads = num_cpus::get().min(12);
    NoIgnore::disregard(&arg.no_ignore)
      .walk(&arg.paths)
      .threads(threads)
      .build_parallel()
  }
  fn produce_item(&self, path: &Path) -> Option<Self::Item> {
    let rules = self.configs.for_path(path);
    if rules.is_empty() {
      return None;
    }
    let lang = rules[0].language;
    let combined = CombinedScan::new(rules);
    let unit = filter_file_interactive(path, lang, ast_grep_core::matcher::MatchAll)?;
    if combined.find(&unit.grep) {
      return Some((unit.path, unit.grep));
    }
    None
  }
  fn consume_items(&self, items: Items<Self::Item>) -> Result<()> {
    self.printer.before_print()?;
    let mut has_error = 0;
    for (path, grep) in items {
      let file_content = grep.root().text().to_string();
      let path = &path;
      let rules = self.configs.for_path(path);
      let combined = CombinedScan::new(rules);
      let matched = combined.scan(&grep);
      for (idx, matches) in matched {
        let rule = &combined.rules[idx];
        if matches!(rule.severity, Severity::Error) {
          has_error += 1;
        }
        match_rule_on_file(path, matches, rule, &file_content, &self.printer)?;
      }
    }
    self.printer.after_print()?;
    if has_error > 0 {
      Err(anyhow::anyhow!(EC::DiagnosticError(has_error)))
    } else {
      Ok(())
    }
  }
}

fn match_rule_on_file(
  path: &Path,
  matches: Vec<NodeMatch<SupportLang>>,
  rule: &RuleConfig<SupportLang>,
  file_content: &String,
  reporter: &impl Printer,
) -> Result<()> {
  let matches = matches.into_iter();
  let file = SimpleFile::new(path.to_string_lossy(), file_content);
  if let Some(fixer) = &rule.fixer {
    let diffs = matches.map(|m| Diff::generate(m, &rule.matcher, fixer));
    reporter.print_rule_diffs(diffs, path, rule)?;
  } else {
    reporter.print_rule(matches, file, rule)?;
  }
  Ok(())
}

struct CombinedScan<'r> {
  rules: Vec<&'r RuleConfig<SupportLang>>,
  kind_rule_mapping: Vec<Vec<usize>>,
}

impl<'r> CombinedScan<'r> {
  fn new(rules: Vec<&'r RuleConfig<SupportLang>>) -> Self {
    let mut mapping = Vec::new();
    for (idx, rule) in rules.iter().enumerate() {
      for kind in &rule
        .matcher
        .potential_kinds()
        .unwrap_or_else(|| panic!("rule `{}` must have kind", &rule.id))
      {
        // NOTE: common languages usually have about several hundred kinds
        // from 200+ ~ 500+, it is okay to waste about 500 * 24 Byte vec size = 12kB
        // see https://github.com/Wilfred/difftastic/tree/master/vendored_parsers
        let k = kind as usize;
        while mapping.len() <= k {
          mapping.push(vec![]);
        }
        mapping[k].push(idx);
      }
    }
    Self {
      rules,
      kind_rule_mapping: mapping,
    }
  }

  fn find(&self, root: &AstGrep<SupportLang>) -> bool {
    for node in root.root().dfs() {
      let kind = node.kind_id() as usize;
      let Some(rule_idx) = self.kind_rule_mapping.get(kind) else {
        continue;
      };
      for &idx in rule_idx {
        let rule = &self.rules[idx];
        if rule.matcher.match_node(node.clone()).is_some() {
          return true;
        }
      }
    }
    false
  }
  fn scan<'a>(
    &self,
    root: &'a AstGrep<SupportLang>,
  ) -> HashMap<usize, Vec<NodeMatch<'a, SupportLang>>> {
    let mut results = HashMap::new();
    for node in root.root().dfs() {
      let kind = node.kind_id() as usize;
      let Some(rule_idx) = self.kind_rule_mapping.get(kind) else {
        continue;
      };
      for &idx in rule_idx {
        let rule = &self.rules[idx];
        if let Some(ret) = rule.matcher.match_node(node.clone()) {
          let matches = results.entry(idx).or_insert_with(Vec::new);
          matches.push(ret);
        }
      }
    }
    results
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use std::fs::File;
  use std::io::Write;
  use tempdir::TempDir;

  const RULE: &str = r#"
id: test
message: Add your rule message here....
severity: error # error, warning, hint, info
language: Rust
rule:
  pattern: Some(123)
"#;

  pub fn create_test_files<'a>(
    names_and_contents: impl IntoIterator<Item = (&'a str, &'a str)>,
  ) -> TempDir {
    let dir = TempDir::new("sgtest").unwrap();
    for (name, contents) in names_and_contents {
      let path = dir.path().join(name);
      let mut file = File::create(path.clone()).unwrap();
      file.write_all(contents.as_bytes()).unwrap();
      file.sync_all().unwrap();
    }
    dir
  }

  #[test]
  fn test_run_with_config() {
    let dir = create_test_files([("sgconfig.yml", "ruleDirs: [rules]")]);
    std::fs::create_dir_all(dir.path().join("rules")).unwrap();
    let mut file = File::create(dir.path().join("rules/test.yml")).unwrap();
    file.write_all(RULE.as_bytes()).unwrap();
    let mut file = File::create(dir.path().join("test.rs")).unwrap();
    file
      .write_all("fn test() { Some(123) }".as_bytes())
      .unwrap();
    file.sync_all().unwrap();
    let arg = ScanArg {
      config: Some(dir.path().join("sgconfig.yml")),
      rule: None,
      report_style: ReportStyle::Rich,
      color: ColorArg::Never,
      no_ignore: vec![],
      interactive: false,
      json: false,
      accept_all: false,
      paths: vec![PathBuf::from(".")],
    };
    assert!(run_with_config(arg).is_ok());
  }
}
